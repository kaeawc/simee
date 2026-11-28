//! The NAT's net thread: guest frames in, host sockets out, and back.
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Checksum, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint};

use super::host::{self, Resolver};
use super::packet::{self, Frame, Link};
use super::shape::{self, Bucket, Downlink, Loss};
use sim_core::network::Conditions;

use super::{Config, Shared, TO_GUEST, lock};

/// Per-connection buffers: guest to host, host to guest. Allocated zeroed, so the pages a
/// connection never fills are never touched.
const TCP_RX: usize = 256 * 1024;
const TCP_TX: usize = 256 * 1024;
const UDP_IDLE: Duration = Duration::from_secs(60);
const DNS_IDLE: Duration = Duration::from_secs(10);
const PING_IDLE: Duration = Duration::from_secs(10);
/// Datagrams read from one socket per pass, so one busy flow can't starve the rest.
const DATAGRAMS_PER_PASS: usize = 64;

/// smoltcp's link: frames from the guest wait in `rx`; frames for it collect in `tx` until the
/// pass hands them over. `limit` is how many more the guest's queue takes this pass.
pub struct Wire {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: Vec<Vec<u8>>,
    limit: Option<usize>,
    /// smoltcp wanted to send and `limit` refused it.
    blocked: bool,
    /// `limit` is the download shaper's room, not the guest's queue: the shaper frees it on
    /// its own clock, so the guest has nothing to wake the thread for.
    by_shaper: bool,
    /// The most frames the download shaper holds at once (`usize::MAX`: no shaper).
    shaper_cap: usize,
    verify: bool,
}

impl Wire {
    /// `verify`: check checksums on frames received (a virtio link never corrupts one).
    pub fn new(verify: bool, limit: Option<usize>) -> Wire {
        Wire { rx: VecDeque::new(), tx: Vec::new(), limit, blocked: false, by_shaper: false, shaper_cap: usize::MAX, verify }
    }

    /// How many more frames the guest's queue takes this pass.
    fn room(&self) -> usize {
        self.limit.map_or(usize::MAX, |l| l.saturating_sub(self.tx.len()))
    }

    fn full(&self) -> bool {
        self.room() == 0
    }

    /// Queues the frames of one datagram built outside smoltcp: all of them, or none when the
    /// guest's queue lacks room for all (a datagram missing a fragment is lost anyway).
    fn push_all(&mut self, frames: &mut Vec<Vec<u8>>) -> bool {
        if frames.len() > self.room() {
            frames.clear();
            self.blocked = true;
            return false;
        }
        self.tx.append(frames);
        true
    }
}

pub struct RxTok(Vec<u8>);
pub struct TxTok<'a>(&'a mut Vec<Vec<u8>>);

impl phy::RxToken for RxTok {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxTok<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut v = vec![0; len];
        let r = f(&mut v);
        self.0.push(v);
        r
    }
}

impl phy::Device for Wire {
    type RxToken<'a> = RxTok;
    type TxToken<'a> = TxTok<'a>;

    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(RxTok, TxTok<'_>)> {
        let f = self.rx.pop_front()?;
        // smoltcp answers some frames while taking them (an ARP reply, a RST) whatever `limit`
        // says; `flush` finds those room. Its TCP ACKs and SYN-ACKs are sent from `transmit`,
        // so `limit` holds them until the queue has room instead of losing them.
        Some((RxTok(f), TxTok(&mut self.tx)))
    }

    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<TxTok<'_>> {
        if self.full() {
            self.blocked = true;
            return None;
        }
        Some(TxTok(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = packet::ETH_HDR + packet::MTU;
        if !self.verify {
            c.checksum.ipv4 = Checksum::Tx;
            c.checksum.tcp = Checksum::Tx;
        }
        c
    }
}

/// A flow is keyed by the guest's source and the destination it asked for.
type Key = (SocketAddrV4, SocketAddrV4);

enum TcpState {
    /// The host connect is in progress; the guest's latest SYN waits for its outcome.
    Connecting(Vec<u8>),
    Open(SocketHandle),
    /// smoltcp is done with the connection (the guest sent FIN), but what the guest sent
    /// before it hasn't all reached the host yet: the rest, and how much of it went.
    Draining(Vec<u8>, usize),
}

struct TcpFlow {
    stream: TcpStream,
    state: TcpState,
    ready: PollFlags,
    /// The host closed its half (read returned 0).
    host_eof: bool,
    /// The host's send buffer is full: wait for POLLOUT before writing again.
    write_blocked: bool,
    /// The guest's FIN was passed on.
    host_shut: bool,
    /// The guest sent FIN (smoltcp's socket reached a state past it).
    guest_fin: bool,
    /// Writing to the host failed while draining.
    dead: bool,
}

impl TcpFlow {
    fn new(stream: TcpStream, state: TcpState) -> TcpFlow {
        TcpFlow {
            stream,
            state,
            ready: PollFlags::empty(),
            host_eof: false,
            write_blocked: false,
            host_shut: false,
            guest_fin: false,
            dead: false,
        }
    }

    /// What to wait for on the host socket. Nothing when there is no room for what it would
    /// deliver: a socket polled for an event the pass can't act on would wake it in a loop.
    fn interest(&self, sockets: &SocketSet<'_>) -> PollFlags {
        match self.state {
            TcpState::Connecting(_) => PollFlags::OUT,
            TcpState::Open(h) => {
                let s = sockets.get::<tcp::Socket>(h);
                let mut ev = PollFlags::empty();
                if !self.host_eof && s.can_send() {
                    ev |= PollFlags::IN;
                }
                if self.write_blocked {
                    ev |= PollFlags::OUT;
                }
                ev
            }
            TcpState::Draining(..) if self.write_blocked => PollFlags::OUT,
            TcpState::Draining(..) => PollFlags::empty(),
        }
    }
}

struct UdpFlow {
    sock: UdpSocket,
    ready: PollFlags,
    idle: Duration,
    deadline: Instant,
    /// DNS (port 53): queries sent and not answered yet; the flow is dropped at the last
    /// answer, so a resolver's one-query sockets don't each hold a host socket for
    /// [`DNS_IDLE`].
    queries: Option<u32>,
}

struct PingFlow {
    sock: UdpSocket,
    guest: Ipv4Addr,
    ready: PollFlags,
    deadline: Instant,
}

#[derive(Clone, Copy)]
enum Slot {
    Tcp(Key),
    Udp(Key),
    Ping((Ipv4Addr, u16)),
}

/// Frames the NAT builds itself (UDP, ICMP), addressed to the guest's MAC once it is known.
struct Emitter {
    gateway_mac: [u8; 6],
    guest_mac: Option<[u8; 6]>,
    ip_id: u16,
    frames: Vec<Vec<u8>>,
}

impl Emitter {
    /// Queues one datagram's frames, all or none (see [`Wire::push_all`]).
    fn emit(&mut self, wire: &mut Wire, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, l4: &[u8]) {
        let Some(guest_mac) = self.guest_mac else { return };
        self.ip_id = self.ip_id.wrapping_add(1);
        let link = Link { gateway_mac: self.gateway_mac, guest_mac };
        packet::ipv4_frames(&mut self.frames, link, src, dst, proto, self.ip_id, l4);
        wire.push_all(&mut self.frames);
    }
}

pub struct Engine {
    cfg: Config,
    shared: Arc<Shared>,
    iface: Interface,
    sockets: SocketSet<'static>,
    wire: Wire,
    tcp: HashMap<Key, TcpFlow>,
    udp: HashMap<Key, UdpFlow>,
    ping: HashMap<(Ipv4Addr, u16), PingFlow>,
    out: Emitter,
    resolver: Resolver,
    /// Room for a datagram's headers ahead of the largest payload.
    scratch: Vec<u8>,
    inbox: VecDeque<Vec<u8>>,
    epoch: Instant,
    /// Run the next pass at once (the guest drained its queue while this one was blocked).
    again: bool,
    /// The conditions last applied, and when they were last read.
    applied: Conditions,
    checked: Option<Instant>,
    /// The shaping those conditions set; none when they set none, and then nothing here
    /// touches it.
    shaping: Option<Shaping>,
    /// `network loss`, shaped or not: the guest's IP packets dropped as they cross the NAT,
    /// from the guest before it acts on them and for the guest as they reach its queue.
    loss: Loss,
}

/// How often an unshaped engine looks at the console's conditions (a shaped one looks every
/// pass): a change reaches the next traffic within this, and the lock isn't taken per pass.
const CONDITIONS_POLL: Duration = Duration::from_millis(50);

/// The state of a shaped link: frames for the guest wait in `down`; frames from the guest
/// wait in `inbox` for `up`'s bucket.
struct Shaping {
    up: Bucket,
    down: Downlink,
    /// When `up` lets the next guest frame through, while `inbox` is held back.
    up_due: Option<Instant>,
}

/// Whether the conditions shape anything.
fn shapes(c: &Conditions) -> bool {
    c.upload_bps > 0.0 || c.download_bps > 0.0 || c.min_latency_ms > 0 || c.max_latency_ms > 0
}

impl Engine {
    pub fn new(cfg: Config, shared: Arc<Shared>) -> Engine {
        let mut wire = Wire::new(false, Some(TO_GUEST));
        let mut c = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(cfg.gateway_mac)));
        c.random_seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        let mut iface = Interface::new(c, &mut wire, smoltcp::time::Instant::from_millis(0));
        iface.update_ip_addrs(|a| {
            for ip in [cfg.gateway, cfg.resolver] {
                let _ = a.push(IpCidr::new(IpAddress::Ipv4(ip), cfg.prefix_len));
            }
        });
        // Accept segments to any destination: each flow's smoltcp socket stands in for it.
        iface.set_any_ip(true);
        Engine {
            loss: Loss::new(cfg.loss_seed.unwrap_or_else(shape::clock_seed)),
            resolver: Resolver::new(cfg.dns.clone()),
            out: Emitter { gateway_mac: cfg.gateway_mac, guest_mac: None, ip_id: 0, frames: Vec::new() },
            cfg,
            shared,
            iface,
            sockets: SocketSet::new(vec![]),
            wire,
            tcp: HashMap::new(),
            udp: HashMap::new(),
            ping: HashMap::new(),
            scratch: vec![0; packet::UDP_HDR + 65536],
            inbox: VecDeque::new(),
            epoch: Instant::now(),
            again: false,
            applied: Conditions::default(),
            checked: None,
            shaping: None,
        }
    }

    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(self.epoch.elapsed().as_micros() as i64)
    }

    pub fn run(mut self, wait: UnixStream) {
        let mut slots: Vec<Slot> = Vec::new();
        let mut revents: Vec<PollFlags> = Vec::new();
        loop {
            let timeout = self.timeout().map(|d| Timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ });
            let woke = {
                let mut fds = Vec::with_capacity(1 + self.tcp.len() + self.udp.len() + self.ping.len());
                slots.clear();
                fds.push(PollFd::new(&wait, PollFlags::IN));
                for (k, f) in &self.tcp {
                    let ev = f.interest(&self.sockets);
                    if !ev.is_empty() {
                        fds.push(PollFd::new(&f.stream, ev));
                        slots.push(Slot::Tcp(*k));
                    }
                }
                // A datagram read while the guest's queue is full would be dropped: they wait
                // in their host sockets until the guest takes frames (the device wakes this).
                if !self.wire.blocked {
                    for (k, f) in &self.udp {
                        fds.push(PollFd::new(&f.sock, PollFlags::IN));
                        slots.push(Slot::Udp(*k));
                    }
                    for (k, f) in &self.ping {
                        fds.push(PollFd::new(&f.sock, PollFlags::IN));
                        slots.push(Slot::Ping(*k));
                    }
                }
                match rustix::event::poll(&mut fds, timeout.as_ref()) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(e) => {
                        eprintln!("emu: net: poll: {e}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                revents.clear();
                revents.extend(fds.iter().map(|f| f.revents()));
                !revents[0].is_empty()
            };
            for (slot, ev) in slots.iter().zip(&revents[1..]) {
                if ev.is_empty() {
                    continue;
                }
                let ready = match slot {
                    Slot::Tcp(k) => self.tcp.get_mut(k).map(|f| &mut f.ready),
                    Slot::Udp(k) => self.udp.get_mut(k).map(|f| &mut f.ready),
                    Slot::Ping(k) => self.ping.get_mut(k).map(|f| &mut f.ready),
                };
                if let Some(r) = ready {
                    *r = *ev;
                }
            }
            if woke {
                let mut b = [0u8; 256];
                while matches!((&wait).read(&mut b), Ok(n) if n > 0) {}
            }
            if self.shared.stop.load(Ordering::Acquire) {
                return;
            }
            self.shared.iterations.fetch_add(1, Ordering::Relaxed);
            self.step();
        }
    }

    /// How long the thread may sleep: smoltcp's next timer (unless the guest's queue is full:
    /// then the device wakes it), or the next idle datagram flow's expiry. None: until woken.
    fn timeout(&mut self) -> Option<Duration> {
        if std::mem::take(&mut self.again) {
            return Some(Duration::ZERO);
        }
        let mut t = if self.wire.blocked {
            None
        } else {
            let now = self.now();
            // A timer due now was due during the pass just run; a floor keeps a socket that
            // can't make progress from spinning the thread.
            self.iface.poll_delay(now, &self.sockets).map(|d| Duration::from_micros(d.total_micros()).max(Duration::from_millis(1)))
        };
        let now = Instant::now();
        // Shaping timers: the next held frame for the guest, and the guest's held frames.
        if let Some(s) = &self.shaping {
            for due in s.down.next_due().into_iter().chain(s.up_due) {
                let left = due.saturating_duration_since(now);
                t = Some(t.map_or(left, |t| t.min(left)));
            }
        }
        let deadlines = self.udp.values().map(|f| f.deadline).chain(self.ping.values().map(|f| f.deadline));
        for d in deadlines {
            let left = d.saturating_duration_since(now);
            t = Some(t.map_or(left, |t| t.min(left)));
        }
        t
    }

    fn step(&mut self) {
        if self.shared.reset.swap(false, Ordering::AcqRel) {
            self.clear();
        }
        self.apply_conditions();
        // Held back by the upload bucket, `inbox` can be nonempty here; unshaped it never is.
        let held_back = !self.inbox.is_empty();
        if !held_back {
            std::mem::swap(&mut *lock(&self.shared.to_host), &mut self.inbox);
        }
        let queued = lock(&self.shared.to_guest).len();
        self.wire.blocked = false;
        match self.shaping.take() {
            None => {
                self.wire.limit = Some(TO_GUEST.saturating_sub(queued));
                self.wire.by_shaper = false;
                self.wire.shaper_cap = usize::MAX;
                while let Some(f) = self.inbox.pop_front() {
                    self.input(f);
                }
            }
            Some(mut s) => {
                let guest = TO_GUEST.saturating_sub(queued + s.down.len());
                self.wire.limit = Some(guest.min(s.down.room()));
                self.wire.by_shaper = s.down.room() < guest;
                self.wire.shaper_cap = s.down.capacity();
                // The upload bucket lets frames through while it isn't in debt; the rest wait
                // here, and then in `to_host`, which holds about a second of the rate and
                // drops past that, as a full link does.
                s.up_due = None;
                while !self.inbox.is_empty() {
                    let now = Instant::now();
                    let wait = s.up.wait(now);
                    if !wait.is_zero() {
                        s.up_due = Some(now + wait);
                        break;
                    }
                    if let Some(f) = self.inbox.pop_front() {
                        s.up.take(now, f.len());
                        self.shared.up_released(f.len());
                        self.input(f);
                    }
                }
                self.shaping = Some(s);
                // Frames the guest queued while `inbox` was held back rang no wakeup (it only
                // rings for the first in an empty queue): take them on the next pass.
                if held_back && self.inbox.is_empty() && !lock(&self.shared.to_host).is_empty() {
                    self.again = true;
                }
            }
        }
        self.poll_iface();
        self.service_connects();
        self.service_tcp();
        self.poll_iface();
        self.reap();
        self.service_datagrams();
        self.expire();
        self.flush();
        #[cfg(test)]
        self.shared.shaper_built.store(self.shaping.is_some(), Ordering::Relaxed);
        self.shared.flows.store(self.tcp.len() + self.udp.len() + self.ping.len(), Ordering::Relaxed);
    }

    fn poll_iface(&mut self) {
        let now = self.now();
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
    }

    /// Hands this pass's frames to the guest's queue and rings the device once.
    fn flush(&mut self) {
        let Some(s) = &mut self.shaping else {
            if self.wire.tx.is_empty() && !self.wire.blocked {
                return;
            }
            let mut q = lock(&self.shared.to_guest);
            let (before, loss) = (q.len(), &mut self.loss);
            q.extend(self.wire.tx.drain(..).filter(|f| !loss.drop_down(f)));
            let any = q.len() > before;
            if self.wire.blocked {
                if q.len() <= TO_GUEST / 2 {
                    self.again = true;
                } else {
                    self.shared.guest_full.store(true, Ordering::Release);
                }
            }
            drop(q);
            if any {
                self.shared.ring_guest();
            }
            return;
        };
        if self.wire.tx.is_empty() && !self.wire.blocked && s.down.is_empty() {
            return;
        }
        let now = Instant::now();
        // The pass was limited to the shaper's room, but smoltcp's replies made while taking a
        // guest frame (an ARP reply, a RST) ignore it, so they may not fit. Control frames take
        // a small reserve past the cap; bulk past it is lost, as at a full link.
        for f in self.wire.tx.drain(..) {
            if packet::is_control(&f) {
                s.down.push_control(now, f);
            } else {
                s.down.push(now, f);
            }
        }
        let mut any = false;
        let mut q = lock(&self.shared.to_guest);
        // Lost past the bottleneck: a dropped frame spent its share of the rate.
        while let Some(f) = s.down.pop_due(now) {
            if !self.loss.drop_down(&f) {
                q.push_back(f);
                any = true;
            }
        }
        // Blocked by the shaper's room: its next due frame wakes the thread, not the guest.
        if self.wire.blocked && !self.wire.by_shaper {
            if q.len() + s.down.len() <= TO_GUEST / 2 {
                self.again = true;
            } else {
                self.shared.guest_full.store(true, Ordering::Release);
            }
        }
        drop(q);
        if any {
            self.shared.ring_guest();
        }
    }

    /// Takes the console's conditions if they changed since the last look.
    fn apply_conditions(&mut self) {
        let at = Instant::now();
        if self.shaping.is_none() && self.checked.is_some_and(|c| at.duration_since(c) < CONDITIONS_POLL) {
            return;
        }
        self.checked = Some(at);
        let c = *lock(&self.cfg.network);
        if c == self.applied {
            return;
        }
        self.applied = c;
        self.loss.set(c.upload_loss_pct, c.download_loss_pct);
        self.shared.shaped.store(shapes(&c), Ordering::Relaxed);
        if shapes(&c) {
            let s = self.shaping.get_or_insert_with(|| Shaping { up: Bucket::new(), down: Downlink::new(), up_due: None });
            s.up.set_rate(c.upload_bps);
            s.down.set(&c);
            let held: usize = self.inbox.iter().map(Vec::len).sum();
            self.shared.set_up_cap(shape::queue_bits(c.upload_bps).map(|b| (b / 8) as usize), held);
        } else if let Some(mut s) = self.shaping.take() {
            // Back to the plain path: what was held reaches the guest now.
            self.wire.tx.splice(0..0, s.down.drain());
            self.shared.set_up_cap(None, 0);
        }
    }

    /// Drops every flow: the guest's stack was reset, so none of its connections exist.
    fn clear(&mut self) {
        self.inbox.clear();
        if let Some(s) = &mut self.shaping {
            s.down.clear();
        }
        self.shared.up_held.store(0, Ordering::Relaxed);
        for f in self.tcp.values() {
            if let TcpState::Open(h) = &f.state {
                let h = *h;
                self.sockets.remove(h);
            }
        }
        self.tcp.clear();
        self.udp.clear();
        self.ping.clear();
        self.wire.rx.clear();
        self.wire.tx.clear();
        self.out.guest_mac = None;
    }

    fn input(&mut self, frame: Vec<u8>) {
        if let Some(m) = packet::src_mac(&frame) {
            self.out.guest_mac = Some(m);
        }
        // Lost on the way out, after the upload's rate took its share.
        if self.loss.drop_up(&frame) {
            return;
        }
        match packet::parse(&frame) {
            Frame::Arp { request, target } => {
                // smoltcp would answer for any address (any-IP): only the gateway's two
                // are here, and the guest's own (a conflict probe) is not.
                if !request || target == self.cfg.gateway || target == self.cfg.resolver {
                    self.wire.rx.push_back(frame);
                }
            }
            Frame::Tcp { src, dst, syn, ack, rst } => self.tcp_in(frame, (src, dst), syn && !ack && !rst),
            Frame::Udp { src, dst, payload } => self.udp_out(src, dst, payload),
            Frame::Ping { src, dst, ident, message } => self.ping_out(src, dst, ident, message),
            Frame::Other => {}
        }
    }

    /// Where a destination the guest asked for is on the host.
    fn host_addr(&mut self, dst: SocketAddrV4) -> Option<SocketAddr> {
        if *dst.ip() == self.cfg.gateway {
            Some(SocketAddr::new(self.cfg.loopback, dst.port()))
        } else if *dst.ip() == self.cfg.resolver {
            let r = self.resolver.server()?;
            Some(if dst.port() == 53 { r } else { SocketAddr::new(r.ip(), dst.port()) })
        } else {
            Some(SocketAddr::V4(dst))
        }
    }

    fn tcp_in(&mut self, frame: Vec<u8>, key: Key, syn: bool) {
        match self.tcp.get_mut(&key) {
            Some(TcpFlow { state: TcpState::Connecting(held), .. }) => {
                if syn {
                    *held = frame;
                }
            }
            Some(_) => self.wire.rx.push_back(frame),
            None if syn => self.open_tcp(key, frame),
            // No flow (one from before a reset): smoltcp answers with RST.
            None => self.wire.rx.push_back(frame),
        }
    }

    fn open_tcp(&mut self, key: Key, syn: Vec<u8>) {
        // Past the caps the SYN finds no socket and smoltcp resets it, as for a refused connect.
        let limits = self.cfg.limits;
        let connecting = self.tcp.values().filter(|f| matches!(f.state, TcpState::Connecting(_))).count();
        if self.tcp.len() >= limits.tcp || connecting >= limits.tcp_connecting {
            self.wire.rx.push_back(syn);
            return;
        }
        let Some(to) = self.host_addr(key.1) else {
            self.wire.rx.push_back(syn);
            return;
        };
        match host::connect(to) {
            Ok((stream, true)) => self.accept_tcp(key, stream, syn),
            Ok((stream, false)) => {
                self.tcp.insert(key, TcpFlow::new(stream, TcpState::Connecting(syn)));
            }
            Err(_) => self.wire.rx.push_back(syn),
        }
    }

    /// The host side connected: a smoltcp socket listening on the destination takes the SYN.
    /// Frames already queued are processed first, so this socket is the only one listening
    /// there when the SYN arrives.
    fn accept_tcp(&mut self, key: Key, stream: TcpStream, syn: Vec<u8>) {
        self.poll_iface();
        let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_RX]), tcp::SocketBuffer::new(vec![0; TCP_TX]));
        s.set_nagle_enabled(false);
        let at = IpListenEndpoint { addr: Some(IpAddress::Ipv4(*key.1.ip())), port: key.1.port() };
        if s.listen(at).is_err() {
            self.wire.rx.push_back(syn);
            return;
        }
        let h = self.sockets.add(s);
        self.wire.rx.push_back(syn);
        self.poll_iface();
        if self.sockets.get::<tcp::Socket>(h).state() == tcp::State::Listen {
            self.sockets.remove(h);
            return;
        }
        self.tcp.insert(key, TcpFlow::new(stream, TcpState::Open(h)));
    }

    fn service_connects(&mut self) {
        let done: Vec<Key> =
            self.tcp.iter().filter(|(_, f)| matches!(f.state, TcpState::Connecting(_)) && !f.ready.is_empty()).map(|(k, _)| *k).collect();
        for k in done {
            let Some(f) = self.tcp.remove(&k) else { continue };
            let TcpState::Connecting(syn) = f.state else { continue };
            match host::connect_result(&f.stream) {
                Ok(()) => self.accept_tcp(k, f.stream, syn),
                // Refused or unreachable: the SYN finds no socket and smoltcp resets it.
                Err(_) => self.wire.rx.push_back(syn),
            }
        }
    }

    /// Moves data between each connection's smoltcp buffers and its host socket.
    fn service_tcp(&mut self) {
        for f in self.tcp.values_mut() {
            let ready = std::mem::replace(&mut f.ready, PollFlags::empty());
            let h = match &mut f.state {
                TcpState::Open(h) => *h,
                TcpState::Draining(rest, at) => {
                    if !f.write_blocked || ready.intersects(PollFlags::OUT | PollFlags::ERR | PollFlags::HUP) {
                        (f.write_blocked, f.dead) = drain(&f.stream, rest, at);
                    }
                    continue;
                }
                TcpState::Connecting(_) => continue,
            };
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            let stream = &f.stream;
            if matches!(s.state(), tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait) {
                f.guest_fin = true;
            }

            // Guest to host, straight from smoltcp's buffer.
            if !f.write_blocked || ready.intersects(PollFlags::OUT | PollFlags::ERR | PollFlags::HUP) {
                f.write_blocked = false;
                while s.can_recv() {
                    match s.recv(|b| io_result((&mut &*stream).write(b))) {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                            f.write_blocked = true;
                            break;
                        }
                        Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                        Ok(Err(_)) => {
                            s.abort();
                            break;
                        }
                    }
                }
            }
            // The guest closed its half and the host has everything it sent before.
            if !f.host_shut
                && !s.can_recv()
                && matches!(s.state(), tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait)
            {
                let _ = stream.shutdown(Shutdown::Write);
                f.host_shut = true;
            }

            // Host to guest, straight into smoltcp's buffer.
            if !f.host_eof && ready.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                while s.can_send() {
                    match s.send(|b| io_result((&mut &*stream).read(b))) {
                        Ok(Ok(0)) => {
                            f.host_eof = true;
                            s.close();
                            break;
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                        Ok(Err(_)) => {
                            f.host_eof = true;
                            s.abort();
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }

    /// Drops connections smoltcp is done with: closed (after any RST went out) or in
    /// TIME-WAIT (the last ACK went out). After the guest's FIN, what it sent before that and
    /// the host hasn't taken yet still goes (smoltcp keeps it, but clears it when TIME-WAIT
    /// ends): the connection drains from a copy, then the host gets its FIN.
    fn reap(&mut self) {
        let sockets = &mut self.sockets;
        self.tcp.retain(|_, f| {
            let h = match &f.state {
                TcpState::Connecting(_) => return true,
                TcpState::Draining(rest, at) => {
                    let done = f.dead || *at >= rest.len();
                    if done && !f.dead && !f.host_shut {
                        let _ = f.stream.shutdown(Shutdown::Write);
                    }
                    return !done;
                }
                TcpState::Open(h) => *h,
            };
            let s = sockets.get_mut::<tcp::Socket>(h);
            let fin = match s.state() {
                tcp::State::TimeWait => true,
                // Without the guest's FIN (it reset the connection, or writing to the host
                // failed) nothing more goes.
                tcp::State::Closed if s.remote_endpoint().is_none() => f.guest_fin,
                _ => return true,
            };
            let mut rest = Vec::new();
            if fin {
                while s.can_recv() {
                    let took = s.recv(|b| {
                        rest.extend_from_slice(b);
                        (b.len(), b.len())
                    });
                    if !matches!(took, Ok(n) if n > 0) {
                        break;
                    }
                }
            }
            sockets.remove(h);
            // Write what the host takes now; the rest waits for it to poll writable.
            let mut at = 0;
            let (blocked, dead) = drain(&f.stream, &rest, &mut at);
            if blocked {
                f.state = TcpState::Draining(rest, at);
                f.write_blocked = true;
                return true;
            }
            if fin && !dead && !f.host_shut {
                let _ = f.stream.shutdown(Shutdown::Write);
            }
            false
        });
    }

    fn udp_out(&mut self, src: SocketAddrV4, dst: SocketAddrV4, payload: &[u8]) {
        let key = (src, dst);
        let now = Instant::now();
        if !self.udp.contains_key(&key) {
            let Some(to) = self.host_addr(dst) else { return };
            if self.udp.len() >= self.cfg.limits.udp {
                evict(&mut self.udp, |f| f.deadline);
            }
            let Ok(sock) = host::udp(to) else { return };
            let dns = dst.port() == 53;
            let idle = if dns { DNS_IDLE } else { UDP_IDLE };
            self.udp.insert(key, UdpFlow { sock, ready: PollFlags::empty(), idle, deadline: now, queries: dns.then_some(0) });
        }
        if let Some(f) = self.udp.get_mut(&key) {
            f.deadline = now + f.idle;
            if let Some(q) = &mut f.queries {
                *q += 1;
            }
            let _ = f.sock.send(payload);
        }
    }

    fn ping_out(&mut self, src: Ipv4Addr, dst: Ipv4Addr, ident: u16, message: &[u8]) {
        if dst == self.cfg.gateway || dst == self.cfg.resolver {
            let m = &mut self.scratch[..message.len()];
            m.copy_from_slice(message);
            packet::echo_reply(m, ident);
            self.out.emit(&mut self.wire, dst, src, packet::PROTO_ICMP, m);
            return;
        }
        let key = (dst, ident);
        let now = Instant::now();
        if !self.ping.contains_key(&key) {
            if self.ping.len() >= self.cfg.limits.ping {
                evict(&mut self.ping, |f| f.deadline);
            }
            let Ok(sock) = host::ping() else { return };
            self.ping.insert(key, PingFlow { sock, guest: src, ready: PollFlags::empty(), deadline: now });
        }
        if let Some(f) = self.ping.get_mut(&key) {
            f.deadline = now + PING_IDLE;
            let _ = f.sock.send_to(message, SocketAddrV4::new(dst, 0));
        }
    }

    /// Replies on the datagram sockets that polled readable, as frames from the address the
    /// guest sent to. A datagram is read only when the guest's queue has room for all its
    /// fragments; the rest wait in their sockets.
    fn service_datagrams(&mut self) {
        let now = Instant::now();
        for (&(src, dst), f) in self.udp.iter_mut() {
            if std::mem::replace(&mut f.ready, PollFlags::empty()).is_empty() {
                continue;
            }
            for _ in 0..DATAGRAMS_PER_PASS {
                let room = self.wire.room();
                if room < packet::MAX_FRAGMENTS
                    && let Ok(n) = f.sock.peek(&mut self.scratch[packet::UDP_HDR..])
                    && packet::fragments(packet::UDP_HDR + n) > room
                {
                    // Too big for the download queue ever: lost, as at a real bottleneck.
                    if packet::fragments(packet::UDP_HDR + n) > self.wire.shaper_cap {
                        let _ = f.sock.recv(&mut self.scratch[packet::UDP_HDR..]);
                        continue;
                    }
                    self.wire.blocked = true;
                    break;
                }
                // ECONNREFUSED (an ICMP port unreachable) or no more data: either ends it.
                let Ok(n) = f.sock.recv(&mut self.scratch[packet::UDP_HDR..]) else { break };
                if n > usize::from(u16::MAX) - packet::UDP_HDR {
                    continue;
                }
                let d = &mut self.scratch[..packet::UDP_HDR + n];
                packet::udp_header(d, dst, src);
                self.out.emit(&mut self.wire, *dst.ip(), *src.ip(), packet::PROTO_UDP, d);
                f.deadline = now + f.idle;
                if let Some(q) = &mut f.queries {
                    *q = q.saturating_sub(1);
                    if *q == 0 {
                        // Answered: `expire` drops it this pass.
                        f.deadline = now;
                        break;
                    }
                }
            }
        }
        for (&(dst, ident), f) in self.ping.iter_mut() {
            if std::mem::replace(&mut f.ready, PollFlags::empty()).is_empty() {
                continue;
            }
            for _ in 0..DATAGRAMS_PER_PASS {
                let room = self.wire.room();
                if room < packet::MAX_FRAGMENTS
                    && let Ok((n, _)) = f.sock.peek_from(&mut self.scratch)
                    && packet::fragments(n) > room
                {
                    if packet::fragments(n) > self.wire.shaper_cap {
                        let _ = f.sock.recv_from(&mut self.scratch);
                        continue;
                    }
                    self.wire.blocked = true;
                    break;
                }
                let Ok((n, from)) = f.sock.recv_from(&mut self.scratch) else { break };
                let b = &self.scratch[..n];
                let Some(m) = packet::strip_ip_header(b) else { continue };
                let with_ip = m.len() != b.len();
                let off = n - m.len();
                if m.len() < packet::ICMP_HDR || m[0] != packet::ICMP_ECHO_REPLY {
                    continue;
                }
                // Darwin hands every ICMP socket every echo reply, with the identifier as
                // sent; Linux hands this socket only its own, with the identifier it chose.
                let theirs = u16::from_be_bytes([m[4], m[5]]);
                if from.ip() != std::net::IpAddr::V4(dst) || (with_ip && theirs != ident) {
                    continue;
                }
                let m = &mut self.scratch[off..n];
                packet::echo_reply(m, ident);
                self.out.emit(&mut self.wire, dst, f.guest, packet::PROTO_ICMP, m);
                f.deadline = now + PING_IDLE;
            }
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.udp.retain(|_, f| f.deadline > now);
        self.ping.retain(|_, f| f.deadline > now);
    }
}

/// Writes what's left of a drained connection to the host, from `at`, until it's all gone or
/// the host's buffer is full: whether it is full, and whether the host failed.
fn drain(stream: &TcpStream, rest: &[u8], at: &mut usize) -> (bool, bool) {
    while *at < rest.len() {
        match (&mut &*stream).write(&rest[*at..]) {
            Ok(0) => return (false, true),
            Ok(n) => *at += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return (true, false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return (false, true),
        }
    }
    (false, false)
}

fn io_result(r: io::Result<usize>) -> (usize, io::Result<usize>) {
    match r {
        Ok(n) => (n, Ok(n)),
        Err(e) => (0, Err(e)),
    }
}

/// Drops the flow closest to expiring, to make room.
fn evict<K: Copy + Eq + std::hash::Hash, V>(flows: &mut HashMap<K, V>, deadline: impl Fn(&V) -> Instant) {
    if let Some(k) = flows.iter().min_by_key(|(_, v)| deadline(v)).map(|(k, _)| *k) {
        flows.remove(&k);
    }
}
