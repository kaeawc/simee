//! The publisher end to end against a fake WHIP server whose answer comes from a str0m receiver
//! on loopback UDP: offer and answer, ICE and DTLS, H.264 over RTP and back, PLI to the encoder,
//! bandwidth estimates, and DELETE when the publisher goes away. The fixtures are ten 96x64
//! frames of ffmpeg's `testsrc` from libx264 (two GOPs), Constrained Baseline and Main.
//!
//! Needs a DTLS backend: Apple targets, or Linux with `--features rust-crypto` (skipped
//! otherwise). An ignored test publishes to a real MediaMTX when one is on PATH.

mod common;

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use common::{BASELINE, MAIN, Viewer, whip_server};
use simee_rtc::h264::{self, NAL_IDR, NAL_PPS, NAL_SPS, nal_type};
use simee_rtc::{Error, KeyframeKind, ProfileLevelId, Publisher, PublisherConfig, PublisherEvent, Sent, WhipOptions};

const WAIT: Duration = Duration::from_secs(10);

fn skip() -> bool {
    if simee_rtc::crypto_available() {
        return false;
    }
    eprintln!("SKIP: no DTLS backend in this build (Linux needs --features rust-crypto)");
    true
}

fn config(url: String, profile: ProfileLevelId) -> PublisherConfig {
    PublisherConfig {
        profile,
        whip: WhipOptions { bearer_token: Some("tok".into()), ..WhipOptions::default() },
        keyframe_interval: Duration::ZERO,
        ..PublisherConfig::new(url)
    }
}

fn connect(url: String, profile: ProfileLevelId) -> (Publisher, Receiver<PublisherEvent>) {
    let (tx, events) = mpsc::channel();
    let publisher = Publisher::connect(config(url, profile), move |e| {
        let _ = tx.send(e);
    })
    .expect("connect");
    (publisher, events)
}

fn wait_event(events: &Receiver<PublisherEvent>, want: impl Fn(&PublisherEvent) -> bool) -> PublisherEvent {
    let end = std::time::Instant::now() + WAIT;
    loop {
        let left = end.saturating_duration_since(std::time::Instant::now());
        match events.recv_timeout(left) {
            Ok(e) if want(&e) => return e,
            Ok(_) => {}
            Err(e) => panic!("no matching event: {e}"),
        }
    }
}

/// The NAL units of each access unit the viewer receives, until `n` of them.
fn received(viewer: &Viewer, n: usize) -> Vec<Vec<Vec<u8>>> {
    (0..n)
        .map(|i| {
            let au = viewer.frames.recv_timeout(WAIT).unwrap_or_else(|e| panic!("frame {i}: {e}"));
            h264::nal_units(&au).into_iter().map(<[u8]>::to_vec).collect()
        })
        .collect()
}

fn owned(units: &[&[u8]]) -> Vec<Vec<u8>> {
    units.iter().map(|n| n.to_vec()).collect()
}

/// Annex B again, the way an encoder hands an access unit over.
fn annex_b(units: &[&[u8]]) -> Vec<u8> {
    units.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
}

#[test]
fn publishes_h264_and_reports_feedback_then_deletes_the_resource() {
    if skip() {
        return;
    }
    let (server, viewers) = whip_server();
    let (mut publisher, events) = connect(server.url("/live/sim/whip"), ProfileLevelId::CONSTRAINED_BASELINE_3_1);
    let viewer = viewers.recv_timeout(WAIT).expect("viewer");
    assert_eq!(publisher.negotiated_profile(), ProfileLevelId::CONSTRAINED_BASELINE_3_1);
    assert_eq!(publisher.resource().to_string(), server.url("/whip/session/1"));
    wait_event(&events, |e| *e == PublisherEvent::Connected);

    let post = server.wait_for("POST", WAIT).expect("POST");
    assert_eq!(post.header("authorization"), Some("Bearer tok"));
    assert!(post.body.contains("a=fmtp:96 ") && post.body.contains("profile-level-id=42e01f"), "{}", post.body);
    assert!(post.body.contains("a=sendonly") && post.body.contains("typ host"), "{}", post.body);
    for line in [
        "a=rtpmap:97 rtx/90000",
        "a=fmtp:97 apt=96",
        "a=rtcp-fb:96 nack",
        "a=rtcp-fb:96 nack pli",
        "a=rtcp-fb:96 ccm fir",
        "a=rtcp-fb:96 transport-cc",
        "a=rtcp-fb:96 goog-remb",
    ] {
        assert!(post.body.lines().any(|l| l.trim() == line), "{line} missing from\n{}", post.body);
    }
    assert!(!post.body.contains("m=audio"), "video only");

    // The second GOP's IDR goes in without its SPS and PPS, as VideoToolbox hands them over.
    let units = h264::access_units(BASELINE);
    assert_eq!(units.len(), 10);
    let mut expected = Vec::new();
    for (i, au) in units.iter().enumerate() {
        let mut au = au.clone();
        if i == 5 {
            assert_eq!(au.iter().map(|n| nal_type(n)).collect::<Vec<_>>(), [NAL_SPS, NAL_PPS, NAL_IDR]);
            au.retain(|n| !matches!(nal_type(n), NAL_SPS | NAL_PPS));
            expected.push(owned(&units[i]));
        } else {
            expected.push(owned(&au));
        }
        let sent = publisher.send(&annex_b(&au), Duration::from_millis(100 * i as u64)).expect("send");
        assert!(matches!(sent, Sent::Queued { keyframe, .. } if keyframe == (i % 5 == 0)), "{i}: {sent:?}");
    }
    assert_eq!(received(&viewer, 10), expected, "every unit back intact, SPS and PPS before each IDR");

    viewer.pli.send(()).expect("pli");
    assert_eq!(
        wait_event(&events, |e| matches!(e, PublisherEvent::KeyframeRequested(_))),
        PublisherEvent::KeyframeRequested(KeyframeKind::Pli)
    );
    let estimate = wait_event(&events, |e| matches!(e, PublisherEvent::BitrateEstimate { .. }));
    assert!(matches!(estimate, PublisherEvent::BitrateEstimate { bps, .. } if bps > 0), "{estimate:?}");

    assert!(server.requests().iter().all(|r| r.method != "DELETE"));
    drop(publisher);
    let delete = server.wait_for("DELETE", WAIT).expect("DELETE on drop");
    assert_eq!((delete.path.as_str(), delete.header("authorization")), ("/whip/session/1", Some("Bearer tok")));
}

#[test]
fn waits_for_a_keyframe_and_negotiates_main() {
    if skip() {
        return;
    }
    let (server, viewers) = whip_server();
    let (mut publisher, _events) = connect(server.url("/whip"), ProfileLevelId::MAIN_3_1);
    let viewer = viewers.recv_timeout(WAIT).expect("viewer");
    assert_eq!(publisher.negotiated_profile(), ProfileLevelId::MAIN_3_1);
    let units = h264::access_units(MAIN);
    // Start mid-GOP: the P frames before the next IDR can't be decoded, so they don't go out.
    for (i, au) in units.iter().enumerate().skip(2) {
        let sent = publisher.send(&annex_b(au), Duration::from_millis(33 * i as u64)).expect("send");
        assert_eq!(sent == Sent::WaitingForKeyframe, i < 5, "{i}: {sent:?}");
    }
    let got = received(&viewer, 5);
    assert_eq!(got[0], owned(&units[5]), "the first unit out is the IDR with its parameter sets");
    assert_eq!(got[4], owned(&units[9]));
    publisher.close().expect("close deletes");
    assert!(server.wait_for("DELETE", WAIT).is_some());
}

#[test]
fn refuses_a_stream_whose_sps_does_not_match_the_session() {
    if skip() {
        return;
    }
    let (server, _viewers) = whip_server();
    let (mut publisher, _events) = connect(server.url("/whip"), ProfileLevelId::CONSTRAINED_BASELINE_3_1);
    let first = annex_b(&h264::access_units(MAIN)[0]);
    match publisher.send(&first, Duration::ZERO) {
        Err(Error::ProfileMismatch { negotiated, stream }) => {
            assert_eq!((negotiated.to_string(), stream.to_string()), ("42e01f".into(), "4d401f".into()));
        }
        other => panic!("{other:?}"),
    }
    // And the other way round. (Levels are checked against the answer's, which str0m's answerer
    // sets to its own 3.1: the unit tests in h264.rs cover a level above it.)
    let (mut publisher, _events) = connect(server.url("/whip"), ProfileLevelId::MAIN_3_1);
    let first = annex_b(&h264::access_units(BASELINE)[0]);
    assert!(matches!(publisher.send(&first, Duration::ZERO), Err(Error::ProfileMismatch { .. })));
}

#[test]
fn an_idr_without_any_parameter_sets_is_refused() {
    if skip() {
        return;
    }
    let (server, _viewers) = whip_server();
    let (mut publisher, _events) = connect(server.url("/whip"), ProfileLevelId::CONSTRAINED_BASELINE_3_1);
    let idr: Vec<&[u8]> = h264::access_units(BASELINE)[0].iter().copied().filter(|n| nal_type(n) == NAL_IDR).collect();
    assert!(matches!(publisher.send(&annex_b(&idr), Duration::ZERO), Err(Error::MissingParameterSets)));
}

#[test]
fn a_refused_answer_deletes_the_resource() {
    if skip() {
        return;
    }
    // A server that takes the POST but answers without a usable video section.
    let server = common::FakeServer::start(|req| match req.method.as_str() {
        "POST" => common::Reply::new(201)
            .header("Location", "/s/9")
            .body("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 0 UDP/TLS/RTP/SAVPF 96\r\n"),
        _ => common::Reply::new(200),
    });
    let err = Publisher::connect(config(server.url("/whip"), ProfileLevelId::CONSTRAINED_BASELINE_3_1), |_| {});
    assert!(matches!(err, Err(Error::Rtc(_))), "{err:?}");
    assert_eq!(server.wait_for("DELETE", WAIT).map(|r| r.path), Some("/s/9".into()));
}

/// Publishes the fixture to a MediaMTX on PATH (`webrtcAddress: :8889`, any path), for a manual
/// check that a real SFU takes the offer; the harness is #267.
#[test]
#[ignore = "needs mediamtx on PATH and running"]
fn publishes_to_a_local_mediamtx() {
    if skip() {
        return;
    }
    let url = std::env::var("SIMEE_WHIP_URL").unwrap_or_else(|_| "http://127.0.0.1:8889/simee-test/whip".into());
    let (mut publisher, events) = connect(url, ProfileLevelId::CONSTRAINED_BASELINE_3_1);
    wait_event(&events, |e| *e == PublisherEvent::Connected);
    for (i, au) in h264::access_units(BASELINE).iter().enumerate() {
        publisher.send(&annex_b(au), Duration::from_millis(100 * i as u64)).expect("send");
        std::thread::sleep(Duration::from_millis(100));
    }
    publisher.close().expect("close");
}
