//! [`Publisher`]: one H.264 video track to a WHIP endpoint.
//!
//! `connect` builds a str0m `Rtc` in RTP mode with exactly one send-only H.264 format (the
//! configured `profile-level-id`, packetization mode 1, RTX, and PLI/FIR/NACK/TWCC/REMB
//! feedback), offers it with one host candidate, POSTs the offer, applies the answer and waits
//! for ICE and DTLS. A thread then owns the `Rtc` and its UDP socket and sleeps on the socket
//! until a datagram, a str0m timeout or a frame arrives (a frame wakes it with a datagram from
//! a second socket, so there is no polling interval).
//!
//! `send` runs on the caller's thread: it splits the access unit, refuses an SPS that doesn't
//! match the negotiated profile, puts the cached SPS/PPS in front of every IDR, packetizes
//! (RFC 6184, FU-A at the MTU) and hands the payloads to the thread, which writes them with
//! their 90 kHz timestamp. str0m answers NACKs from its RTX cache and runs TWCC bandwidth
//! estimation; keyframe requests (PLI/FIR, throttled) and estimates reach the encoder through the
//! event callback, on the publisher's thread: keep it short.
//!
//! With [`Publisher::connect_with_input`] the offer also carries a reliable, ordered data channel
//! labelled `simee-input` (a peer that opens one with that label is accepted too): the thread
//! hands its messages to an [`InputSession`], which delivers events to the session's sink and
//! answers on the channel (`docs/streaming.md`, "Input over a data channel").
//!
//! Dropping a publisher (or `close`) stops the thread and sends DELETE for the WHIP resource.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpAnswer;
use str0m::channel::ChannelId;
use str0m::media::{Direction, KeyframeRequestKind, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::RtpWrite;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::datachannel::{self, InputSession, Reply};
use crate::h264::{self, ParameterSets, ProfileLevelId};
use crate::rtp::{self, Payload};
use crate::sdp;
use crate::url::Url;
use crate::whip::{WhipClient, WhipOptions, WhipSession};
use crate::{Error, Result};

/// The payload types the offer uses: H.264 and its retransmissions (RFC 4588).
const PT_H264: u8 = 96;
const PT_RTX: u8 = 97;
/// A UDP datagram is never larger than this on the paths WebRTC uses.
const RECV_BUFFER: usize = 2048;

#[derive(Clone, Debug)]
pub struct PublisherConfig {
    /// The WHIP endpoint, e.g. `http://127.0.0.1:8889/<path>/whip` for MediaMTX.
    pub endpoint: String,
    pub whip: WhipOptions,
    /// The `profile-level-id` to offer: the encoder's (see [`ProfileLevelId::from_sps`]). Every
    /// SPS sent is checked against what the session negotiates.
    pub profile: ProfileLevelId,
    /// The largest RTP payload.
    pub mtu: usize,
    /// Where TWCC bandwidth estimation starts, in bits per second; `None` turns it off (REMB
    /// estimates from the server are still reported).
    pub start_bitrate_bps: Option<u64>,
    /// What bandwidth estimation probes up to.
    pub max_bitrate_bps: u64,
    /// Report at most one keyframe request per interval; requests inside it are coalesced into
    /// one at its end, so none is lost.
    pub keyframe_interval: Duration,
    /// The address for the host candidate. Default: the one the system routes to the endpoint's
    /// host from (loopback for a loopback endpoint).
    pub local_ip: Option<IpAddr>,
    /// How long ICE and DTLS may take after the answer.
    pub connect_timeout: Duration,
}

impl PublisherConfig {
    pub fn new(endpoint: impl Into<String>) -> PublisherConfig {
        PublisherConfig {
            endpoint: endpoint.into(),
            whip: WhipOptions::default(),
            profile: ProfileLevelId::CONSTRAINED_BASELINE_3_1,
            mtu: rtp::DEFAULT_MTU,
            start_bitrate_bps: Some(1_000_000),
            max_bitrate_bps: 8_000_000,
            keyframe_interval: Duration::from_millis(500),
            local_ip: None,
            connect_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyframeKind {
    /// Picture Loss Indication: a viewer lost or never had a reference picture.
    Pli,
    /// Full Intra Request.
    Fir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitrateSource {
    /// The publisher's own transport-wide congestion control estimate.
    Twcc,
    /// The receiver's REMB report.
    Remb,
}

/// What the encoder hears from the publisher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublisherEvent {
    /// ICE and DTLS are up; media flows.
    Connected,
    /// Encode an IDR next.
    KeyframeRequested(KeyframeKind),
    /// The bandwidth available, in bits per second: the encoder's target bitrate.
    BitrateEstimate { bps: u64, source: BitrateSource },
    /// The connection is gone (ICE failed, or the peer closed it). Publish again with a new
    /// publisher for a new WHIP resource.
    Disconnected,
}

/// What [`Publisher::send`] did with an access unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// Packetized and queued.
    Queued { packets: usize, keyframe: bool },
    /// Dropped: no IDR has been sent yet, and a viewer can't decode anything before one.
    WaitingForKeyframe,
}

/// Whether this build has a DTLS backend: Apple targets always, Linux with the `rust-crypto`
/// feature.
pub fn crypto_available() -> bool {
    crypto_provider().is_some()
}

#[cfg(any(target_vendor = "apple", feature = "rust-crypto"))]
fn crypto_provider() -> Option<Arc<str0m::config::CryptoProvider>> {
    Some(Arc::new(str0m::crypto::from_feature_flags()))
}

#[cfg(not(any(target_vendor = "apple", feature = "rust-crypto")))]
fn crypto_provider() -> Option<Arc<str0m::config::CryptoProvider>> {
    None
}

enum Command {
    Frame { timestamp: u32, wallclock: Instant, payloads: Vec<Payload> },
    Stop,
}

/// A live WHIP session publishing one H.264 track.
pub struct Publisher {
    whip: WhipClient,
    session: WhipSession,
    negotiated: ProfileLevelId,
    mtu: usize,
    sets: ParameterSets,
    sent_keyframe: bool,
    timestamp_base: u32,
    commands: Sender<Command>,
    waker: UdpSocket,
    wake_to: SocketAddr,
    alive: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    closed: bool,
}

impl Publisher {
    /// Offer, POST, answer, connect. Returns once media can flow; on any failure after the POST
    /// the WHIP resource is deleted again.
    pub fn connect(config: PublisherConfig, on_event: impl FnMut(PublisherEvent) + Send + 'static) -> Result<Publisher> {
        Self::open(config, None, Box::new(on_event))
    }

    /// [`connect`](Self::connect), with the `simee-input` data channel in the offer: the viewer's
    /// input reaches `input`'s sink on the publisher's thread, and its replies go back on the
    /// channel. A server must carry the channel to the viewer for this to do anything (a direct
    /// peer does; MediaMTX relays media only).
    pub fn connect_with_input(
        config: PublisherConfig,
        input: InputSession,
        on_event: impl FnMut(PublisherEvent) + Send + 'static,
    ) -> Result<Publisher> {
        Self::open(config, Some(input), Box::new(on_event))
    }

    /// [`connect_with_input`](Self::connect_with_input), except that a server which refuses the
    /// data channel is offered the stream again without it: MediaMTX answers an offer with an
    /// `m=application` section `406 unsupported media 'application'`. Only that answer to the
    /// POST (a 406 whose body names `application`) is retried, once; any other error, and the
    /// retry's own, is returned as it is. The second value is why the channel was dropped
    /// (`None`: the channel is in the offer the server accepted).
    pub fn connect_with_input_or_without(
        config: PublisherConfig,
        input: InputSession,
        on_event: impl FnMut(PublisherEvent) + Send + 'static,
    ) -> Result<(Publisher, Option<String>)> {
        let on_event = Arc::new(Mutex::new(on_event));
        let forward = || -> Box<dyn FnMut(PublisherEvent) + Send> {
            let on_event = on_event.clone();
            Box::new(move |event| {
                if let Ok(mut f) = on_event.lock() {
                    f(event);
                }
            })
        };
        match Self::open(config.clone(), Some(input), forward()) {
            Err(Error::Status { status: 406, method: "POST", body }) if body.contains("application") => {
                let note = format!("the WHIP server refused the input data channel (406 {})", body.trim());
                Self::open(config, None, forward()).map(|p| (p, Some(note)))
            }
            other => other.map(|p| (p, None)),
        }
    }

    fn open(config: PublisherConfig, input: Option<InputSession>, on_event: Box<dyn FnMut(PublisherEvent) + Send>) -> Result<Publisher> {
        if config.mtu <= 2 {
            return Err(Error::InvalidInput(format!("MTU {} is too small", config.mtu)));
        }
        let whip = WhipClient::new(&config.endpoint, config.whip.clone())?;
        let crypto = crypto_provider()
            .ok_or_else(|| Error::Unsupported("no DTLS backend in this build: enable simee-rtc's `rust-crypto` feature on Linux".into()))?;

        let ip = local_ip(whip.endpoint(), config.local_ip)?;
        let socket = UdpSocket::bind(SocketAddr::new(ip, 0))?;
        let local = socket.local_addr()?;
        let waker = UdpSocket::bind(SocketAddr::new(ip, 0))?;
        let waker_addr = waker.local_addr()?;

        let mut rtc_config = RtcConfig::new().set_crypto_provider(crypto).set_rtp_mode(true).clear_codecs();
        rtc_config.codec_config().add_h264(PT_H264.into(), Some(PT_RTX.into()), true, config.profile.as_u32());
        if let Some(start) = config.start_bitrate_bps {
            rtc_config = rtc_config.enable_bwe(Some(Bitrate::bps(start)));
        }
        let mut rtc = rtc_config.build(Instant::now());
        let candidate = Candidate::host(local, "udp").map_err(|e| Error::Rtc(e.to_string()))?;
        if rtc.add_local_candidate(candidate).is_none() {
            return Err(Error::Rtc(format!("str0m refused the host candidate {local}")));
        }
        let mut changes = rtc.sdp_api();
        let mid = changes.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        if input.is_some() {
            // Reliable and ordered (str0m's default): a lost or reordered touch-up is a stuck finger.
            changes.add_channel(datachannel::LABEL.into());
        }
        let (offer, pending) = changes.apply().ok_or_else(|| Error::Rtc("str0m produced no offer".into()))?;

        let session = whip.publish(&offer.to_sdp_string())?;
        // From here on the server holds a resource: every failure deletes it (a built
        // publisher does that when dropped).
        let (rtc, negotiated) = match Self::finish(&config, rtc, pending, &session) {
            Ok(v) => v,
            Err(e) => {
                let _ = whip.delete(&session.resource);
                return Err(e);
            }
        };
        let (commands, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let driver = Driver {
            rtc,
            socket,
            local,
            waker_addr,
            commands: rx,
            mid,
            pt: PT_H264.into(),
            seq: u64::from(random_u32() as u16),
            on_event,
            input,
            input_channel: None,
            replies: Vec::new(),
            keyframes: Throttle::new(config.keyframe_interval),
            max_bitrate: config.max_bitrate_bps,
            bwe: config.start_bitrate_bps.is_some(),
            alive: alive.clone(),
            ready: Some(ready_tx),
        };
        let thread = match thread::Builder::new().name("simee-rtc".into()).spawn(move || driver.run()) {
            Ok(t) => t,
            Err(e) => {
                let _ = whip.delete(&session.resource);
                return Err(e.into());
            }
        };
        let publisher = Publisher {
            whip,
            session,
            negotiated,
            mtu: config.mtu,
            sets: ParameterSets::default(),
            sent_keyframe: false,
            timestamp_base: random_u32(),
            commands,
            waker,
            wake_to: local,
            alive,
            thread: Some(thread),
            closed: false,
        };
        match ready_rx.recv_timeout(config.connect_timeout) {
            Ok(()) => Ok(publisher),
            Err(RecvTimeoutError::Timeout) => {
                Err(Error::Timeout(format!("ICE/DTLS to {} within {:?} of the answer", publisher.session.resource, config.connect_timeout)))
            }
            Err(RecvTimeoutError::Disconnected) => Err(Error::Rtc("the connection failed before it was established".into())),
        }
    }

    fn finish(
        config: &PublisherConfig,
        mut rtc: Rtc,
        pending: str0m::change::SdpPendingOffer,
        session: &WhipSession,
    ) -> Result<(Rtc, ProfileLevelId)> {
        let negotiated = sdp::answered_h264_profile(&session.answer)
            .ok_or_else(|| Error::Rtc("the answer has no H.264 video section: the server refused the track".into()))?;
        if negotiated.profile_idc != config.profile.profile_idc {
            return Err(Error::ProfileMismatch { negotiated, stream: config.profile });
        }
        let answer = SdpAnswer::from_sdp_string(&session.answer).map_err(|e| Error::Rtc(e.to_string()))?;
        rtc.sdp_api().accept_answer(pending, answer).map_err(|e| Error::Rtc(e.to_string()))?;
        Ok((rtc, negotiated))
    }

    /// Publish one access unit: Annex B NAL units of one picture, with its presentation time
    /// (from any fixed origin: only differences matter). Refuses an SPS that doesn't match the
    /// negotiated profile, and an IDR with no SPS/PPS seen yet.
    pub fn send(&mut self, access_unit: &[u8], pts: Duration) -> Result<Sent> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let nals = h264::nal_units(access_unit);
        for stream in nals.iter().filter_map(|n| ProfileLevelId::from_sps(n)) {
            if !self.negotiated.accepts(stream) {
                return Err(Error::ProfileMismatch { negotiated: self.negotiated, stream });
            }
        }
        self.sets.observe(&nals);
        let keyframe = h264::is_keyframe(&nals);
        if !keyframe && !self.sent_keyframe {
            return Ok(Sent::WaitingForKeyframe);
        }
        let units = self.sets.assemble(&nals).ok_or(Error::MissingParameterSets)?;
        let payloads = rtp::packetize(&units, self.mtu)?;
        if payloads.is_empty() {
            return Ok(Sent::Queued { packets: 0, keyframe });
        }
        let packets = payloads.len();
        let timestamp = rtp::timestamp(self.timestamp_base, pts);
        self.commands.send(Command::Frame { timestamp, wallclock: Instant::now(), payloads }).map_err(|_| Error::Closed)?;
        self.wake();
        self.sent_keyframe |= keyframe;
        Ok(Sent::Queued { packets, keyframe })
    }

    /// The `profile-level-id` the server answered.
    pub fn negotiated_profile(&self) -> ProfileLevelId {
        self.negotiated
    }

    /// The session's WHIP resource (DELETE target).
    pub fn resource(&self) -> &Url {
        &self.session.resource
    }

    /// Whether the media thread is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Stop the media and DELETE the WHIP resource, reporting a failed DELETE (dropping does the
    /// same, silently).
    pub fn close(mut self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let _ = self.commands.send(Command::Stop);
        self.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.whip.delete(&self.session.resource)
    }

    fn wake(&self) {
        let _ = self.waker.send_to(&[0], self.wake_to);
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        // Never unwind out of Drop (that aborts when already panicking): a DELETE that panics
        // is a failed DELETE.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.shutdown()));
    }
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("resource", &self.session.resource.to_string())
            .field("negotiated", &self.negotiated.to_string())
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

/// The host candidate's address: configured, loopback for a loopback endpoint, else the source
/// address the system routes to the endpoint from (a connected UDP socket sends nothing).
fn local_ip(endpoint: &Url, configured: Option<IpAddr>) -> Result<IpAddr> {
    if let Some(ip) = configured {
        return Ok(ip);
    }
    if let Some(ip) = endpoint.loopback_ip() {
        return Ok(ip);
    }
    let target = (endpoint.host(), endpoint.port())
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::InvalidInput(format!("{endpoint}: the host resolves to no address")))?;
    let any = match target {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let probe = UdpSocket::bind(SocketAddr::new(any, 0))?;
    probe.connect(target)?;
    Ok(probe.local_addr()?.ip())
}

/// Not for cryptography (str0m makes the SRTP keys): an RTP sequence number and timestamp origin,
/// which RFC 3550 §5.1 wants random.
fn random_u32() -> u32 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(Instant::now().elapsed().as_nanos());
    h.finish() as u32
}

/// Keyframe requests, at most one per interval; one arriving inside the interval is reported
/// when it ends.
#[derive(Debug)]
struct Throttle {
    interval: Duration,
    last: Option<Instant>,
    pending: Option<KeyframeKind>,
}

impl Throttle {
    fn new(interval: Duration) -> Throttle {
        Throttle { interval, last: None, pending: None }
    }

    fn request(&mut self, now: Instant, kind: KeyframeKind) -> Option<KeyframeKind> {
        let kind = match self.pending {
            Some(KeyframeKind::Fir) => KeyframeKind::Fir,
            _ => kind,
        };
        if self.last.is_none_or(|l| now.duration_since(l) >= self.interval) {
            self.last = Some(now);
            self.pending = None;
            Some(kind)
        } else {
            self.pending = Some(kind);
            None
        }
    }

    fn due(&self) -> Option<Instant> {
        self.pending.and(self.last.map(|l| l + self.interval))
    }

    fn poll(&mut self, now: Instant) -> Option<KeyframeKind> {
        let kind = self.pending?;
        if self.due().is_some_and(|d| now >= d) {
            self.last = Some(now);
            self.pending = None;
            return Some(kind);
        }
        None
    }
}

struct Driver {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    waker_addr: SocketAddr,
    commands: Receiver<Command>,
    mid: Mid,
    pt: Pt,
    seq: u64,
    on_event: Box<dyn FnMut(PublisherEvent) + Send>,
    input: Option<InputSession>,
    /// The open `simee-input` channel.
    input_channel: Option<ChannelId>,
    /// Replies waiting to be written to it.
    replies: Vec<Reply>,
    keyframes: Throttle,
    max_bitrate: u64,
    bwe: bool,
    alive: Arc<AtomicBool>,
    ready: Option<SyncSender<()>>,
}

impl Driver {
    fn run(mut self) {
        let mut buf = vec![0u8; RECV_BUFFER];
        let mut connected = false;
        'run: loop {
            loop {
                match self.commands.try_recv() {
                    Ok(Command::Frame { timestamp, wallclock, payloads }) => {
                        self.write(timestamp, wallclock, payloads);
                    }
                    Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                        self.rtc.disconnect();
                        let _ = self.drain(&mut connected);
                        break 'run;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            self.poll_input(Instant::now());
            let Ok(mut deadline) = self.drain(&mut connected) else { break };
            if !self.rtc.is_alive() {
                break;
            }
            let now = Instant::now();
            if let Some(kind) = self.keyframes.poll(now) {
                (self.on_event)(PublisherEvent::KeyframeRequested(kind));
            }
            if let Some(due) = self.keyframes.due() {
                deadline = deadline.min(due);
            }
            if let Some(due) = self.input.as_ref().and_then(InputSession::next_due) {
                deadline = deadline.min(due);
            }
            let wait = deadline.saturating_duration_since(now);
            if wait.is_zero() {
                if self.rtc.handle_input(Input::Timeout(now)).is_err() {
                    break;
                }
                continue;
            }
            // A zero read timeout is an error; anything shorter than a millisecond rounds up.
            if self.socket.set_read_timeout(Some(wait.max(Duration::from_millis(1)))).is_err() {
                break;
            }
            let input_ok = match self.socket.recv_from(&mut buf) {
                Ok((_, from)) if from == self.waker_addr => true,
                Ok((n, from)) => match buf[..n].try_into() {
                    Ok(contents) => {
                        let receive = Receive { proto: Protocol::Udp, source: from, destination: self.local, contents };
                        self.rtc.handle_input(Input::Receive(Instant::now(), receive)).is_ok()
                    }
                    // Not STUN, DTLS, RTP or RTCP: ignore it.
                    Err(_) => true,
                },
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now())).is_ok()
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => true,
                // ICMP port unreachable surfaces as ConnectionRefused/Reset on some systems: the
                // peer isn't there (yet); ICE decides when to give up.
                Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => true,
                Err(_) => false,
            };
            if !input_ok {
                break;
            }
        }
        if let Some(input) = self.input.as_mut() {
            input.release_all();
        }
        self.alive.store(false, Ordering::Release);
        if connected {
            (self.on_event)(PublisherEvent::Disconnected);
        }
    }

    fn write(&mut self, timestamp: u32, wallclock: Instant, payloads: Vec<Payload>) {
        // Bring str0m's clock to now so the pacer sends these promptly.
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
        let mut api = self.rtc.direct_api();
        let Some(tx) = api.stream_tx_by_mid(self.mid, None) else { return };
        for p in payloads {
            let write = RtpWrite::new(self.pt, self.seq.into(), timestamp, wallclock, p.data).marker(p.marker).nackable(true);
            tx.write_rtp(write);
            self.seq += 1;
        }
    }

    /// Deliver coalesced moves that are due, and write their replies.
    fn poll_input(&mut self, now: Instant) {
        if let Some(input) = self.input.as_mut() {
            input.poll(now, &mut self.replies);
        }
        self.write_replies();
    }

    /// Write the pending replies to the input channel. One the channel can't buffer is dropped:
    /// replies are acks and errors, and a viewer that floods the channel loses its own.
    fn write_replies(&mut self) {
        if self.replies.is_empty() {
            return;
        }
        let replies = std::mem::take(&mut self.replies);
        let Some(mut channel) = self.input_channel.and_then(|id| self.rtc.channel(id)) else { return };
        for reply in replies {
            let _ = channel.write(false, reply.to_json().as_bytes());
        }
    }

    /// Send what str0m has queued and handle its events, until it wants time to pass.
    fn drain(&mut self, connected: &mut bool) -> Result<Instant, ()> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return Ok(at),
                Ok(Output::Transmit(t)) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(event)) => self.on(event, connected),
                Err(_) => return Err(()),
            }
        }
    }

    fn on(&mut self, event: Event, connected: &mut bool) {
        match event {
            Event::Connected => {
                *connected = true;
                if self.bwe {
                    self.rtc.bwe().set_bitrate(Bitrate::ZERO, Bitrate::bps(self.max_bitrate));
                }
                if let Some(ready) = self.ready.take() {
                    let _ = ready.try_send(());
                }
                (self.on_event)(PublisherEvent::Connected);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                if *connected {
                    *connected = false;
                    (self.on_event)(PublisherEvent::Disconnected);
                }
            }
            Event::KeyframeRequest(req) => {
                let kind = match req.kind {
                    KeyframeRequestKind::Pli => KeyframeKind::Pli,
                    KeyframeRequestKind::Fir => KeyframeKind::Fir,
                };
                if let Some(kind) = self.keyframes.request(Instant::now(), kind) {
                    (self.on_event)(PublisherEvent::KeyframeRequested(kind));
                }
            }
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                (self.on_event)(PublisherEvent::BitrateEstimate { bps: estimate.as_u64(), source: BitrateSource::Twcc });
            }
            Event::EgressBitrateEstimate(BweKind::Remb { estimate, .. }) => {
                (self.on_event)(PublisherEvent::BitrateEstimate { bps: estimate.as_u64(), source: BitrateSource::Remb });
            }
            // Ours (str0m reports the channels it opened too) or one the peer opened with the label.
            Event::ChannelOpen(id, label) if label == datachannel::LABEL && self.input_channel.is_none() => {
                if let Some(input) = self.input.as_mut() {
                    // A new viewer starts clean: lift whatever the last one left down.
                    input.release_all();
                    self.input_channel = Some(id);
                    self.replies.push(input.hello());
                    self.write_replies();
                }
            }
            Event::ChannelData(data) if Some(data.id) == self.input_channel => {
                if let Some(input) = self.input.as_mut() {
                    input.receive(Instant::now(), data.binary, &data.data, &mut self.replies);
                    self.write_replies();
                }
            }
            Event::ChannelClose(id) if Some(id) == self.input_channel => {
                self.input_channel = None;
                if let Some(input) = self.input.as_mut() {
                    input.release_all();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttles_keyframe_requests_without_losing_one() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Throttle::new(ms(500));
        assert_eq!(t.request(t0, KeyframeKind::Pli), Some(KeyframeKind::Pli));
        assert_eq!(t.request(t0 + ms(100), KeyframeKind::Pli), None);
        assert_eq!(t.request(t0 + ms(200), KeyframeKind::Fir), None);
        assert_eq!(t.request(t0 + ms(300), KeyframeKind::Pli), None, "FIR stays the pending kind");
        assert_eq!(t.due(), Some(t0 + ms(500)));
        assert_eq!(t.poll(t0 + ms(499)), None);
        assert_eq!(t.poll(t0 + ms(500)), Some(KeyframeKind::Fir));
        assert_eq!((t.poll(t0 + ms(2000)), t.due()), (None, None), "nothing pending");
        assert_eq!(t.request(t0 + ms(1100), KeyframeKind::Pli), Some(KeyframeKind::Pli));
    }

    #[test]
    fn the_host_candidate_follows_the_endpoint() {
        let lo = Url::parse("http://localhost:8889/x/whip").expect("url");
        assert_eq!(local_ip(&lo, None).expect("loopback"), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let v6 = Url::parse("http://[::1]:8889/x/whip").expect("url");
        assert_eq!(local_ip(&v6, None).expect("v6"), IpAddr::V6(Ipv6Addr::LOCALHOST));
        let lan = Url::parse("http://192.0.2.1:8889/x/whip").expect("url");
        let pinned = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!(local_ip(&lan, Some(pinned)).expect("configured"), pinned);
    }

    #[test]
    fn refuses_before_any_traffic() {
        let insecure = PublisherConfig::new("http://192.0.2.1:8889/x/whip");
        assert!(matches!(Publisher::connect(insecure, |_| {}), Err(Error::Insecure(_))));
        let tls = PublisherConfig::new("https://127.0.0.1:8889/x/whip");
        assert!(matches!(Publisher::connect(tls, |_| {}), Err(Error::Unsupported(_))));
        let mtu = PublisherConfig { mtu: 2, ..PublisherConfig::new("http://127.0.0.1:9/x") };
        assert!(matches!(Publisher::connect(mtu, |_| {}), Err(Error::InvalidInput(_))));
    }
}
