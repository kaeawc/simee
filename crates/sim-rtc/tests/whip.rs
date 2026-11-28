//! The WHIP client against a fake server on `std::net`: what it sends, and what it makes of each
//! answer.

mod common;

use std::time::Duration;

use common::{FakeServer, Reply};
use simee_rtc::whip::Trickle;
use simee_rtc::{Error, WhipClient, WhipOptions};

const ANSWER: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";

fn options(token: Option<&str>) -> WhipOptions {
    WhipOptions { bearer_token: token.map(str::to_owned), timeout: Duration::from_secs(5), ..WhipOptions::default() }
}

#[test]
fn posts_the_offer_and_follows_location_and_etag() {
    let server = FakeServer::start(|req| match req.method.as_str() {
        "POST" => Reply::new(201).header("Location", "session/7").header("ETag", "\"e1\"").body(ANSWER),
        "PATCH" if req.header("if-match") == Some("\"e1\"") => Reply::new(204).header("ETag", "\"e2\""),
        "PATCH" => Reply::new(412),
        "DELETE" => Reply::new(200),
        _ => Reply::new(405),
    });
    let client = WhipClient::new(&server.url("/live/sim/whip"), options(Some("t0k3n"))).expect("client");
    let mut session = client.publish("v=0\r\noffer\r\n").expect("publish");
    assert_eq!(session.answer, ANSWER);
    assert_eq!(session.resource.to_string(), server.url("/live/sim/session/7"));
    assert_eq!(session.etag.as_deref(), Some("\"e1\""));

    let frag = simee_rtc::sdp::trickle_fragment("u", "p", "0", &["candidate:1 1 udp 1 127.0.0.1 9 typ host".into()], true);
    assert_eq!(client.patch(&mut session, &frag).expect("patch"), Trickle::Accepted);
    assert_eq!(session.etag.as_deref(), Some("\"e2\""), "the new ETag replaces the old");
    let stale = client.patch(&mut session, &frag);
    assert!(matches!(stale, Err(Error::Status { status: 412, method: "PATCH", .. })), "{stale:?}");
    client.delete(&session.resource).expect("delete");

    let reqs = server.requests();
    let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(methods, ["POST", "PATCH", "PATCH", "DELETE"]);
    let post = &reqs[0];
    assert_eq!((post.path.as_str(), post.body.as_str()), ("/live/sim/whip", "v=0\r\noffer\r\n"));
    assert_eq!(post.header("content-type"), Some("application/sdp"));
    assert!(reqs.iter().all(|r| r.header("authorization") == Some("Bearer t0k3n")), "{reqs:?}");
    let patch = &reqs[1];
    assert_eq!(patch.header("content-type"), Some("application/trickle-ice-sdpfrag"));
    assert_eq!((patch.path.as_str(), patch.body.as_str()), ("/live/sim/session/7", frag.as_str()));
    assert_eq!(reqs[3].path, "/live/sim/session/7");
}

#[test]
fn a_server_that_does_not_trickle_is_not_an_error() {
    let server = FakeServer::start(|req| match req.method.as_str() {
        "POST" => Reply::new(201).header("Location", "/s/1").body(ANSWER),
        _ => Reply::new(405),
    });
    let client = WhipClient::new(&server.url("/whip"), options(None)).expect("client");
    let mut session = client.publish("v=0\r\n").expect("publish");
    assert_eq!(session.etag, None);
    assert_eq!(client.patch(&mut session, "a=end-of-candidates\r\n").expect("patch"), Trickle::Unsupported);
    assert_eq!(server.requests()[1].header("if-match"), None, "no ETag, no If-Match");
    assert!(server.requests()[0].header("authorization").is_none());
}

#[test]
fn refuses_answers_that_are_not_201_with_a_location() {
    let server = FakeServer::start(|req| match req.path.as_str() {
        "/s" if req.method == "DELETE" => Reply::new(200),
        "/busy" => Reply::new(503).body("no publishers allowed"),
        "/ok-not-created" => Reply::new(200).header("Location", "/s").body(ANSWER),
        "/no-location" => Reply::new(201).body(ANSWER),
        "/not-sdp" => Reply::new(201).header("Location", "/s").body("<html>"),
        _ => Reply::new(404),
    });
    let publish = |path: &str| WhipClient::new(&server.url(path), options(None)).expect("client").publish("v=0\r\n");
    match publish("/busy") {
        Err(Error::Status { status: 503, method: "POST", body }) => assert_eq!(body, "no publishers allowed"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(publish("/ok-not-created"), Err(Error::Status { status: 200, .. })));
    assert!(matches!(publish("/no-location"), Err(Error::Protocol(_))));
    assert!(matches!(publish("/not-sdp"), Err(Error::Protocol(_))));
    let deletes: Vec<String> = server.requests().into_iter().filter(|r| r.method == "DELETE").map(|r| r.path).collect();
    assert_eq!(deletes, ["/s"], "the not-SDP 201 created a resource the client must delete, and only that one");
}

#[test]
fn debug_output_never_shows_the_bearer_token() {
    let opts = options(Some("s3cr3t-token"));
    let client = WhipClient::new("http://127.0.0.1:1/w", opts.clone()).expect("client");
    let config = simee_rtc::PublisherConfig { whip: opts.clone(), ..simee_rtc::PublisherConfig::new("http://127.0.0.1:1/w") };
    for text in [format!("{opts:?}"), format!("{opts:#?}"), format!("{client:?}"), format!("{config:?}")] {
        assert!(!text.contains("s3cr3t-token"), "{text}");
        assert!(text.contains("redacted"), "{text}");
    }
    assert!(format!("{:?}", WhipOptions::default()).contains("None"));
}

#[test]
fn follows_redirects_but_never_to_plaintext_elsewhere() {
    let server = FakeServer::start(|req| match req.path.as_str() {
        "/old" => Reply::new(307).header("Location", "/new"),
        "/new" => Reply::new(201).header("Location", "/s/1").body(ANSWER),
        "/away" => Reply::new(307).header("Location", "http://192.0.2.1/whip"),
        "/loop" => Reply::new(308).header("Location", "/loop"),
        "/elsewhere" => Reply::new(201).header("Location", "http://192.0.2.1/s/1").body(ANSWER),
        _ => Reply::new(404),
    });
    let client = |path: &str| WhipClient::new(&server.url(path), options(Some("secret"))).expect("client");
    let session = client("/old").publish("v=0\r\n").expect("redirected");
    assert_eq!(session.resource.path(), "/s/1");
    assert!(matches!(client("/away").publish("v=0\r\n"), Err(Error::Insecure(_))));
    assert!(matches!(client("/loop").publish("v=0\r\n"), Err(Error::Protocol(_))));
    assert!(matches!(client("/elsewhere").publish("v=0\r\n"), Err(Error::Insecure(_))), "resource off-host");
    let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
    let expected = ["/old", "/new", "/away", "/loop", "/loop", "/loop", "/loop", "/elsewhere"];
    assert_eq!(paths, expected, "the client stops at each refusal: no follow-up POST, no DELETE of an off-host resource");
}

#[test]
fn delete_treats_a_missing_session_as_done() {
    let server = FakeServer::start(|req| match req.path.as_str() {
        "/gone" => Reply::new(404),
        _ => Reply::new(500).body("boom"),
    });
    let client = WhipClient::new(&server.url("/whip"), options(None)).expect("client");
    client.delete(&simee_rtc::Url::parse(&server.url("/gone")).expect("url")).expect("404 is fine");
    let err = client.delete(&simee_rtc::Url::parse(&server.url("/broken")).expect("url"));
    assert!(matches!(err, Err(Error::Status { status: 500, method: "DELETE", .. })), "{err:?}");
}
