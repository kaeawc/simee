//! The `simee-input` data channel end to end, with no browser: the publisher (the offerer) and a
//! str0m answerer behind a fake WHIP server, over loopback UDP. The answerer plays the viewer: it
//! waits for the channel the publisher opens, reads the `hello`, sends input messages, and hands
//! the replies to the test. The publisher's fake sink must see the events in order (moves
//! coalesced), and every `t` must come back in an ack or a pong with the server's receive time.
//! The Chromium version of this (a real browser tapping through the channel, input-to-photon
//! latency in the video) belongs with #268.
//!
//! Needs a DTLS backend: Apple targets, or Linux with `--features rust-crypto` (skipped
//! otherwise).

mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{BASELINE, FakeServer, Reply as HttpReply};
use simee_rtc::datachannel::{
    self, Body, Button, ButtonPhase, ErrorCode, FrameSize, KeyPhase, Message, Orientation, Point, Posture, Reply, TouchPhase,
};
use simee_rtc::{InputConfig, InputEvent, InputSession, InputSink, ProfileLevelId, Publisher, PublisherConfig, h264};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

const WAIT: Duration = Duration::from_secs(10);
const FRAME: FrameSize = FrameSize::new(96, 64);

fn skip() -> bool {
    if simee_rtc::crypto_available() {
        return false;
    }
    eprintln!("SKIP: no DTLS backend in this build (Linux needs --features rust-crypto)");
    true
}

/// The viewer end: the answerer's channel, as the test drives it.
struct Viewer {
    /// The label of each channel that opened.
    opened: Receiver<String>,
    /// Text to write on the input channel.
    send: Sender<String>,
    /// Each message the publisher wrote, as text.
    replies: Receiver<String>,
    /// How many video samples arrived.
    media: Arc<AtomicUsize>,
}

impl Viewer {
    fn reply(&self) -> Reply {
        let text = self.replies.recv_timeout(WAIT).expect("a reply");
        Reply::decode(&text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }
}

/// A WHIP server whose POST answers with a str0m peer that plays the viewer.
fn whip_viewer() -> (FakeServer, Receiver<Viewer>) {
    let (viewers_tx, viewers) = mpsc::channel();
    let viewers_tx = Mutex::new(viewers_tx);
    let server = FakeServer::start(move |req| match req.method.as_str() {
        "POST" => {
            let Ok(offer) = SdpOffer::from_sdp_string(&req.body) else {
                return HttpReply::new(400).body("bad offer");
            };
            let (answer, viewer) = answerer(offer);
            let _ = viewers_tx.lock().expect("lock").send(viewer);
            HttpReply::new(201).header("Content-Type", "application/sdp").header("Location", "/whip/session/1").body(&answer)
        }
        "DELETE" => HttpReply::new(200),
        _ => HttpReply::new(405),
    });
    (server, viewers)
}

fn answerer(offer: SdpOffer) -> (String, Viewer) {
    let provider = Arc::new(str0m::crypto::from_feature_flags());
    let mut rtc = RtcConfig::new().set_crypto_provider(provider).build(Instant::now());
    let socket = UdpSocket::bind("127.0.0.1:0").expect("udp");
    let local = socket.local_addr().expect("addr");
    rtc.add_local_candidate(Candidate::host(local, "udp").expect("candidate"));
    let answer = rtc.sdp_api().accept_offer(offer).expect("answer");
    let (opened_tx, opened) = mpsc::channel();
    let (send, send_rx) = mpsc::channel();
    let (replies_tx, replies) = mpsc::channel();
    let media = Arc::new(AtomicUsize::new(0));
    let counter = media.clone();
    thread::spawn(move || run_viewer(rtc, socket, local, &opened_tx, &send_rx, &replies_tx, &counter));
    (answer.to_sdp_string(), Viewer { opened, send, replies, media })
}

fn run_viewer(
    mut rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    opened: &Sender<String>,
    outgoing: &Receiver<String>,
    replies: &Sender<String>,
    media: &AtomicUsize,
) {
    let mut buf = vec![0u8; 2048];
    let mut channel: Option<ChannelId> = None;
    let started = Instant::now();
    while rtc.is_alive() && started.elapsed() < Duration::from_secs(60) {
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(Event::ChannelOpen(id, label))) => {
                    if label == datachannel::LABEL {
                        channel = Some(id);
                    }
                    let _ = opened.send(label);
                }
                Ok(Output::Event(Event::ChannelData(d))) => {
                    let _ = replies.send(String::from_utf8_lossy(&d.data).into_owned());
                }
                Ok(Output::Event(Event::MediaData(_))) => {
                    media.fetch_add(1, Ordering::SeqCst);
                }
                Ok(Output::Event(_)) => {}
                Err(_) => return,
            }
        };
        let mut wrote = false;
        if let Some(id) = channel {
            while let Ok(text) = outgoing.try_recv() {
                let Some(mut ch) = rtc.channel(id) else { break };
                assert!(ch.write(false, text.as_bytes()).expect("write"), "the channel took {text}");
                wrote = true;
            }
        }
        if wrote {
            continue;
        }
        let wait = timeout.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(1), Duration::from_millis(5));
        let _ = socket.set_read_timeout(Some(wait));
        let input = match socket.recv_from(&mut buf) {
            Ok((n, source)) => match buf[..n].try_into() {
                Ok(contents) => Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents }),
                Err(_) => continue,
            },
            Err(_) => Input::Timeout(Instant::now()),
        };
        if rtc.handle_input(input).is_err() {
            return;
        }
    }
}

/// A sink that records what reaches it, and refuses a back button like an iPhone.
#[derive(Clone, Default)]
struct FakeSink(Arc<Mutex<Vec<InputEvent>>>);

impl InputSink for FakeSink {
    fn input(&mut self, event: &InputEvent) -> Result<(), String> {
        if let InputEvent::Button { button: Button::Back, .. } = event {
            return Err("no back button on this device".into());
        }
        self.0.lock().map_err(|e| e.to_string())?.push(event.clone());
        Ok(())
    }
}

impl FakeSink {
    fn wait_for(&self, n: usize) -> Vec<InputEvent> {
        let end = Instant::now() + WAIT;
        loop {
            let got = self.0.lock().expect("lock").clone();
            if got.len() >= n || Instant::now() > end {
                return got;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn wall_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_secs_f64() * 1000.0
}

fn point(x: f64, y: f64) -> Point {
    Point { x, y, frame: FRAME }
}

fn touch(seq: u64, t: Option<f64>, phase: TouchPhase, x: f64) -> Message {
    Message { seq: Some(seq), t, body: Body::Touch { pointer: 0, phase, point: Some(point(x, 0.5)) } }
}

fn input(seq: u64, t: Option<f64>, event: InputEvent) -> Message {
    Message { seq: Some(seq), t, body: Body::Input(event) }
}

fn annex_b(units: &[&[u8]]) -> Vec<u8> {
    units.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
}

#[test]
fn input_reaches_the_sink_in_order_and_every_timestamp_comes_back() {
    if skip() {
        return;
    }
    let (server, viewers) = whip_viewer();
    let sink = FakeSink::default();
    // A long move interval makes the coalescing deterministic: the first move goes through, the
    // rest wait, and the lift delivers the latest of them first.
    let session = InputSession::new(sink.clone(), InputConfig { min_move_interval: Duration::from_secs(5), ..InputConfig::default() });
    let config = PublisherConfig { profile: ProfileLevelId::CONSTRAINED_BASELINE_3_1, ..PublisherConfig::new(server.url("/whip")) };
    let mut publisher = Publisher::connect_with_input(config, session, |_| {}).expect("connect");
    let viewer = viewers.recv_timeout(WAIT).expect("viewer");

    let offer = server.wait_for("POST", WAIT).expect("POST").body;
    assert!(offer.lines().any(|l| l.starts_with("m=application ") && l.contains("webrtc-datachannel")), "{offer}");
    assert!(offer.contains("m=video "), "video still offered: {offer}");

    assert_eq!(viewer.opened.recv_timeout(WAIT).expect("channel open"), datachannel::LABEL);
    assert_eq!(viewer.reply(), Reply::Hello { versions: vec![datachannel::VERSION] });

    // Video and input share the connection.
    for (i, au) in h264::access_units(BASELINE).iter().enumerate() {
        publisher.send(&annex_b(au), Duration::from_millis(100 * i as u64)).expect("send");
    }

    let before = wall_ms();
    let script = [
        touch(1, Some(1.0), TouchPhase::Down, 0.1),
        touch(2, None, TouchPhase::Move, 0.2),
        touch(3, None, TouchPhase::Move, 0.3),
        touch(4, None, TouchPhase::Move, 0.4),
        touch(5, None, TouchPhase::Move, 0.5),
        Message { seq: Some(6), t: Some(6.0), body: Body::Touch { pointer: 0, phase: TouchPhase::Up, point: None } },
        input(7, None, InputEvent::Key { phase: KeyPhase::Down, usage: 0x28 }),
        input(8, None, InputEvent::Text("hi ✓".into())),
        input(9, None, InputEvent::Button { button: Button::Home, phase: ButtonPhase::Press }),
        input(10, Some(10.0), InputEvent::Button { button: Button::Back, phase: ButtonPhase::Press }),
        input(11, None, InputEvent::Rotate(Orientation::LandscapeRight)),
        input(12, None, InputEvent::Fold(Posture::HalfOpen)),
        input(13, None, InputEvent::Wheel { point: point(0.5, 0.5), dx: 0.0, dy: -40.0 }),
    ];
    for m in &script {
        viewer.send.send(m.to_json()).expect("send");
    }
    // A key by name, refusals, and the echo, as raw text the way a browser would write them.
    for text in [
        r#"{"v":1,"seq":14,"kind":"key","phase":"up","key":"Enter"}"#,
        r#"{"v":2,"seq":15,"kind":"touch"}"#,
        r#"{"v":1,"seq":16,"kind":"teleport"}"#,
        r#"{"v":1,"seq":17,"kind":"touch","phase":"move","id":42,"x":0.5,"y":0.5,"w":96,"h":64}"#,
        "not json",
    ] {
        viewer.send.send(text.into()).expect("send");
    }
    let sent_ping = Instant::now();
    viewer.send.send(Message { seq: Some(20), t: Some(20.5), body: Body::Ping }.to_json()).expect("ping");

    let mut replies = Vec::new();
    let rtt = loop {
        let r = viewer.reply();
        if let Reply::Pong { .. } = r {
            replies.push(r);
            break sent_ping.elapsed();
        }
        replies.push(r);
    };
    eprintln!("ping round trip over loopback: {rtt:?}");

    let expected = [
        InputEvent::Touch { pointer: 0, phase: TouchPhase::Down, point: point(0.1, 0.5) },
        InputEvent::Touch { pointer: 0, phase: TouchPhase::Move, point: point(0.2, 0.5) },
        InputEvent::Touch { pointer: 0, phase: TouchPhase::Move, point: point(0.5, 0.5) },
        InputEvent::Touch { pointer: 0, phase: TouchPhase::Up, point: point(0.5, 0.5) },
        InputEvent::Key { phase: KeyPhase::Down, usage: 0x28 },
        InputEvent::Text("hi ✓".into()),
        InputEvent::Button { button: Button::Home, phase: ButtonPhase::Press },
        InputEvent::Rotate(Orientation::LandscapeRight),
        InputEvent::Fold(Posture::HalfOpen),
        InputEvent::Wheel { point: point(0.5, 0.5), dx: 0.0, dy: -40.0 },
        InputEvent::Key { phase: KeyPhase::Up, usage: 0x28 },
    ];
    assert_eq!(sink.wait_for(expected.len()), expected, "in order; moves 3 and 4 coalesced into 5");

    let summary: Vec<(Option<u64>, &str)> = replies
        .iter()
        .map(|r| match r {
            Reply::Ack { seq, .. } => (*seq, "ack"),
            Reply::Pong { seq, .. } => (*seq, "pong"),
            Reply::Error { seq, code, .. } => (*seq, code.as_str()),
            Reply::Hello { .. } => (None, "hello"),
        })
        .collect();
    assert_eq!(
        summary,
        [
            (Some(1), "ack"),
            (Some(6), "ack"),
            (Some(10), ErrorCode::Sink.as_str()),
            (Some(15), ErrorCode::Version.as_str()),
            (Some(16), ErrorCode::Kind.as_str()),
            (Some(17), ErrorCode::Pointer.as_str()),
            (None, ErrorCode::Json.as_str()),
            (Some(20), "pong"),
        ]
    );
    let after = wall_ms();
    for r in &replies {
        match r {
            Reply::Ack { seq, t, rx, done } => {
                assert_eq!(*t, seq.map(|s| s as f64), "the viewer's t echoed");
                assert!(before <= *rx && rx <= done && *done <= after, "{r:?}");
            }
            Reply::Pong { t, rx, .. } => assert!(*t == Some(20.5) && before <= *rx && *rx <= after, "{r:?}"),
            _ => {}
        }
    }

    let end = Instant::now() + WAIT;
    while viewer.media.load(Ordering::SeqCst) == 0 && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(viewer.media.load(Ordering::SeqCst) > 0, "video arrived beside the channel");
    publisher.close().expect("close");
}

/// Neighbouring behaviour: a plain `connect` offers video only, with no data channel.
#[test]
fn connect_without_input_offers_no_channel() {
    if skip() {
        return;
    }
    let (server, _viewers) = common::whip_server();
    let publisher = Publisher::connect(PublisherConfig::new(server.url("/whip")), |_| {}).expect("connect");
    let offer = server.wait_for("POST", WAIT).expect("POST").body;
    assert!(!offer.contains("m=application"), "{offer}");
    drop(publisher);
}
