//! `net-segment`: the second NIC's frames, relayed between linked emu processes (#539).
//!
//! A segment is a directory of Unix datagram sockets, one per member (`m<k>.sock`), and every
//! datagram is one Ethernet frame. A member's frames go straight to the member that owns the
//! destination MAC (the addresses are derived, `sim_core::segment`); broadcast, multicast and
//! unicast to a MAC nobody owns go to every other member, so mDNS and Nearby's Wi-Fi LAN medium
//! cross between guests. There is no central process: a member that isn't running is a cable
//! that isn't plugged in, and its frames are dropped, as on a hub. Nothing is learned and
//! nothing is stored, so a member restarting (a new boot, a crash) needs no one's help to rejoin.
//!
//! One thread per member, blocked in `poll(2)` on its socket and a wakeup: an idle segment costs
//! no CPU. Sends are non-blocking and run on the device's worker thread, so a slow peer drops its
//! own frames and never stalls the guest or another peer.
//!
//! Sockets are the process's own, in a directory made `0700`: no more reach than the console and
//! display sockets next to it. A frame is accepted whatever its source address, since only
//! processes of the same user can write to the directory, so [`SegmentPort::open`] refuses a
//! directory that is someone else's, or a link or a file, and takes back one of this user's that
//! is open to others.
use std::collections::VecDeque;
use std::io;
#[cfg(unix)]
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use sim_core::segment::{self, SegmentSpec};

use crate::virtio::net::{Backend, RxReady};

/// Frames held for the guest; past this the newest drop, as at a full link.
const TO_GUEST: usize = 256;
/// The largest frame taken. The OS limits a datagram first (macOS: `net.local.dgram.maxdgram`,
/// 2048 by default), which holds a standard 1500-byte MTU with room to spare.
const MAX_FRAME: usize = 16 * 1024;
/// An Ethernet header: destination, source, ethertype.
const ETH_HEADER: usize = 14;
/// How long a list of the directory's sockets serves floods before it is read again.
const PEER_REFRESH: Duration = Duration::from_secs(1);
/// Socket buffers asked for, best effort: macOS's defaults hold about two frames.
#[cfg(unix)]
const SOCKET_BUFFER: usize = 1 << 20;
/// Datagrams read per pass, so a flood can't hold the thread from its stop signal.
#[cfg(unix)]
const READS_PER_PASS: usize = 64;

/// Where a frame the guest sent goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum Route {
    /// The member that owns the destination MAC.
    Unicast(u8),
    /// Every other member: a group address, or a unicast MAC nobody here owns.
    Flood,
}

/// Where `frame`, sent by member `own` of `subnet`, goes; `None` drops it (shorter than an
/// Ethernet header, or addressed to the sender itself).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn route(frame: &[u8], subnet: u8, own: u8) -> Option<Route> {
    let dst: [u8; 6] = frame.get(..6).filter(|_| frame.len() >= ETH_HEADER)?.try_into().ok()?;
    if segment::is_group(&dst) {
        return Some(Route::Flood);
    }
    match segment::member_of_mac(dst, subnet) {
        Some(k) if k == own => None,
        Some(k) => Some(Route::Unicast(k)),
        None => Some(Route::Flood),
    }
}

/// Whether a frame that arrived is one to hand the guest: whole, and not from its own MAC (a
/// frame that came back round would loop).
#[cfg_attr(not(unix), allow(dead_code))]
fn acceptable(frame: &[u8], own_mac: [u8; 6]) -> bool {
    frame.len() >= ETH_HEADER && frame.len() <= MAX_FRAME && frame[6..12] != own_mac
}

/// The members whose sockets are in `names` (a directory listing), other than `own`, in order.
#[cfg_attr(not(unix), allow(dead_code))]
fn peers_in<'a>(names: impl IntoIterator<Item = &'a str>, own: u8) -> Vec<u8> {
    let mut peers: Vec<u8> = names.into_iter().filter_map(segment::member_of_socket).filter(|k| *k != own).collect();
    peers.sort_unstable();
    peers.dedup();
    peers
}

/// What to do about the segment directory as found.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum DirCheck {
    /// Ours and private.
    Fine,
    /// Ours, but open to group or others: `chmod 0700` it.
    Tighten,
    Refuse(String),
}

/// Judges a directory by what `lstat` says: its type, owner and mode, against the user running
/// emu. Anyone who can write to the directory can forge frames or take a member's socket, so
/// only a directory of this user's counts.
#[cfg_attr(not(unix), allow(dead_code))]
fn judge_dir(is_dir: bool, owner: u32, mode: u32, me: u32) -> DirCheck {
    if !is_dir {
        DirCheck::Refuse("is not a directory (or is a link to one)".into())
    } else if owner != me {
        DirCheck::Refuse(format!("belongs to uid {owner}, not to this user (uid {me})"))
    } else if mode & 0o077 != 0 {
        DirCheck::Tighten
    } else {
        DirCheck::Fine
    }
}

/// Makes sure `dir` is this user's own private directory, as it was made or found.
#[cfg(unix)]
fn secure_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let meta = std::fs::symlink_metadata(dir)?;
    let me = rustix::process::geteuid().as_raw();
    match judge_dir(meta.file_type().is_dir(), meta.uid(), meta.mode(), me) {
        DirCheck::Fine => Ok(()),
        DirCheck::Tighten => std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)),
        DirCheck::Refuse(why) => Err(io::Error::new(ErrorKind::PermissionDenied, format!("the segment directory {} {why}", dir.display()))),
    }
}

/// What the device's worker and the segment's thread share.
struct Shared {
    to_guest: Mutex<VecDeque<Vec<u8>>>,
    ready: Mutex<Option<RxReady>>,
    #[cfg(unix)]
    stop: std::sync::atomic::AtomicBool,
    /// Frames the guest sent that went nowhere (no such peer, a full peer, an oversize frame),
    /// frames dropped at a full queue, and thread wakeups: for tests and diagnostics.
    #[cfg(unix)]
    counters: Counters,
}

#[cfg(unix)]
#[derive(Default)]
struct Counters {
    sent: std::sync::atomic::AtomicU64,
    tx_dropped: std::sync::atomic::AtomicU64,
    rx_dropped: std::sync::atomic::AtomicU64,
    wakeups: std::sync::atomic::AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The segment NIC's backend, for the virtio-net device on the segment's slot.
#[cfg(unix)]
pub struct SegmentPort {
    spec: SegmentSpec,
    shared: Arc<Shared>,
    sock: Arc<std::os::unix::net::UnixDatagram>,
    /// The wakeup's write end and its read end (until the thread takes it).
    wake: std::os::unix::net::UnixStream,
    wait: Option<std::os::unix::net::UnixStream>,
    thread: Option<std::thread::JoinHandle<()>>,
    peers: Vec<u8>,
    peers_at: Option<Instant>,
    refresh: Duration,
}

#[cfg(unix)]
impl SegmentPort {
    /// Joins the segment: makes its directory (`0700`) and binds this member's socket there. A
    /// socket file left by a member that died is replaced; one a running process still answers
    /// on is an error, since two guests can't share an address.
    pub fn open(spec: &SegmentSpec) -> io::Result<SegmentPort> {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::net::{UnixDatagram, UnixStream};

        spec.validate().map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&spec.dir)?;
        secure_dir(&spec.dir)?;
        let path = spec.socket();
        let sock = match UnixDatagram::bind(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                // A datagram socket that nobody listens on refuses a connect.
                if UnixDatagram::unbound()?.connect(&path).is_ok() {
                    return Err(io::Error::new(
                        ErrorKind::AddrInUse,
                        format!("member {} of the segment in {} is already running", spec.member, spec.dir.display()),
                    ));
                }
                std::fs::remove_file(&path)?;
                UnixDatagram::bind(&path)?
            }
            Err(e) => return Err(e),
        };
        sock.set_nonblocking(true)?;
        // Best effort: the defaults are small, and a refusal only costs headroom.
        let _ = rustix::net::sockopt::set_socket_recv_buffer_size(&sock, SOCKET_BUFFER);
        let _ = rustix::net::sockopt::set_socket_send_buffer_size(&sock, SOCKET_BUFFER);
        let (wake, wait) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wait.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            to_guest: Mutex::new(VecDeque::new()),
            ready: Mutex::new(None),
            stop: std::sync::atomic::AtomicBool::new(false),
            counters: Counters::default(),
        });
        Ok(SegmentPort {
            spec: spec.clone(),
            shared,
            sock: Arc::new(sock),
            wake,
            wait: Some(wait),
            thread: None,
            peers: Vec::new(),
            peers_at: None,
            refresh: PEER_REFRESH,
        })
    }

    /// The members other than this one whose sockets are in the directory now, cached for
    /// [`PEER_REFRESH`].
    fn peers(&mut self) -> &[u8] {
        if self.peers_at.is_none_or(|at| at.elapsed() >= self.refresh) {
            let names: Vec<String> = std::fs::read_dir(&self.spec.dir)
                .map(|d| d.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
                .unwrap_or_default();
            self.peers = peers_in(names.iter().map(String::as_str), self.spec.member);
            self.peers_at = Some(Instant::now());
        }
        &self.peers
    }

    fn send_to(&self, member: u8, frame: &[u8]) {
        use std::sync::atomic::Ordering::Relaxed;
        match self.sock.send_to(frame, self.spec.socket_of(member)) {
            Ok(_) => self.shared.counters.sent.fetch_add(1, Relaxed),
            // Not running (no socket, or a stale file), full, or too big for the OS: dropped, as
            // by a cable that isn't plugged in.
            Err(_) => self.shared.counters.tx_dropped.fetch_add(1, Relaxed),
        };
    }

    #[cfg(test)]
    fn with_refresh(mut self, refresh: Duration) -> SegmentPort {
        self.refresh = refresh;
        self
    }

    #[cfg(test)]
    fn counter(&self, f: impl Fn(&Counters) -> &std::sync::atomic::AtomicU64) -> u64 {
        f(&self.shared.counters).load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The segment thread: datagrams in, frames for the guest out.
#[cfg(unix)]
fn run(sock: &std::os::unix::net::UnixDatagram, mut wait: std::os::unix::net::UnixStream, shared: &Shared, own_mac: [u8; 6]) {
    use std::io::Read;
    use std::sync::atomic::Ordering::{Acquire, Relaxed};

    use rustix::event::{PollFd, PollFlags};

    let mut buf = vec![0u8; MAX_FRAME + 1];
    loop {
        let mut fds = [PollFd::new(&wait, PollFlags::IN), PollFd::new(sock, PollFlags::IN)];
        match rustix::event::poll(&mut fds, None) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                eprintln!("emu: net-segment: poll: {e}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        shared.counters.wakeups.fetch_add(1, Relaxed);
        if shared.stop.load(Acquire) {
            return;
        }
        let _ = wait.read(&mut [0u8; 8]);
        let mut delivered = false;
        for _ in 0..READS_PER_PASS {
            match sock.recv(&mut buf) {
                Ok(n) if acceptable(&buf[..n], own_mac) => {
                    let mut q = lock(&shared.to_guest);
                    if q.len() >= TO_GUEST {
                        shared.counters.rx_dropped.fetch_add(1, Relaxed);
                    } else {
                        q.push_back(buf[..n].to_vec());
                        delivered = true;
                    }
                }
                Ok(_) => {
                    shared.counters.rx_dropped.fetch_add(1, Relaxed);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if delivered && let Some(r) = lock(&shared.ready).as_ref() {
            r.ring();
        }
    }
}

#[cfg(unix)]
impl Backend for SegmentPort {
    fn attach(&mut self, ready: RxReady) {
        *lock(&self.shared.ready) = Some(ready);
        // A new activation (the guest rebooted or reset the device) starts with an empty link.
        lock(&self.shared.to_guest).clear();
        if let Some(wait) = self.wait.take() {
            let (shared, sock, mac) = (self.shared.clone(), self.sock.clone(), self.spec.mac());
            let spawned = std::thread::Builder::new().name("virtio-net-segment".into()).spawn(move || {
                crate::device_thread();
                run(&sock, wait, &shared, mac);
            });
            match spawned {
                Ok(h) => self.thread = Some(h),
                Err(e) => eprintln!("emu: net-segment: no relay thread ({e}); the segment is link only"),
            }
        }
    }

    fn transmit(&mut self, frame: &[u8]) {
        match route(frame, self.spec.subnet, self.spec.member) {
            None => {}
            Some(Route::Unicast(k)) => self.send_to(k, frame),
            Some(Route::Flood) => {
                let peers = self.peers().to_vec();
                for k in peers {
                    self.send_to(k, frame);
                }
            }
        }
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        lock(&self.shared.to_guest).pop_front()
    }
}

#[cfg(unix)]
impl Drop for SegmentPort {
    fn drop(&mut self) {
        if let Some(h) = self.thread.take() {
            self.shared.stop.store(true, std::sync::atomic::Ordering::Release);
            // A full socket buffer means a wakeup is already pending.
            let _ = (&self.wake).write(&[1]);
            let _ = h.join();
        }
        // Leave the directory as a member that isn't running looks.
        let _ = std::fs::remove_file(self.spec.socket());
    }
}

/// No segment where Unix datagram sockets aren't: no value can be built, so the device is not
/// added and emu says why.
#[cfg(not(unix))]
pub struct SegmentPort(std::convert::Infallible);

#[cfg(not(unix))]
impl SegmentPort {
    pub fn open(_spec: &SegmentSpec) -> io::Result<SegmentPort> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "net-segment needs Unix datagram sockets, which this host's emu doesn't use yet"))
    }
}

#[cfg(not(unix))]
impl Backend for SegmentPort {
    fn attach(&mut self, _ready: RxReady) {
        match self.0 {}
    }

    fn transmit(&mut self, _frame: &[u8]) {
        match self.0 {}
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        match self.0 {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::virtio::Doorbell;

    /// Whether `dir` holds a socket for some member: running, or left behind.
    fn has_members(dir: &Path) -> bool {
        std::fs::read_dir(dir).is_ok_and(|d| d.flatten().any(|e| e.file_name().to_str().and_then(segment::member_of_socket).is_some()))
    }

    /// Short on purpose: a macOS temp dir would leave too little of `sun_path`.
    fn dir(name: &str) -> PathBuf {
        let d = PathBuf::from(format!("/tmp/es-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn spec(dir: &Path, subnet: u8, member: u8) -> SegmentSpec {
        SegmentSpec { dir: dir.to_path_buf(), subnet, member }
    }

    /// A running member: its port and the bell its thread rings.
    struct Member {
        port: SegmentPort,
        bell: Arc<Doorbell>,
    }

    fn join(dir: &Path, member: u8) -> Member {
        let mut port = SegmentPort::open(&spec(dir, 3, member)).expect("open").with_refresh(Duration::ZERO);
        let bell = Arc::new(Doorbell::default());
        port.attach(RxReady::new(bell.clone()));
        Member { port, bell }
    }

    /// An Ethernet frame to `dst` from `src`, with a payload that names it.
    fn frame(dst: [u8; 6], src: [u8; 6], payload: &str) -> Vec<u8> {
        let mut f = dst.to_vec();
        f.extend_from_slice(&src);
        f.extend_from_slice(&[0x08, 0x00]);
        f.extend_from_slice(payload.as_bytes());
        f
    }

    const MDNS_V4: [u8; 6] = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb];

    impl Member {
        fn mac(&self) -> [u8; 6] {
            segment::mac_for(3, self.port.spec.member)
        }

        /// The next frame for the guest, waiting up to two seconds for the relay to deliver it.
        fn next(&mut self) -> Option<Vec<u8>> {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(f) = self.port.receive() {
                    return Some(f);
                }
                if Instant::now() >= until {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Whether a frame arrives within `ms` (for asserting that none does).
        fn nothing_within(&mut self, ms: u64) -> bool {
            std::thread::sleep(Duration::from_millis(ms));
            self.port.receive().is_none()
        }
    }

    #[test]
    fn routes_group_addresses_to_all_and_owned_macs_to_their_owner() {
        let own = segment::mac_for(3, 0);
        let to = |dst: [u8; 6]| route(&frame(dst, own, "x"), 3, 0);
        assert_eq!(to([0xff; 6]), Some(Route::Flood), "broadcast");
        assert_eq!(to(MDNS_V4), Some(Route::Flood), "mDNS");
        assert_eq!(to([0x33, 0x33, 0, 0, 0, 0xfb]), Some(Route::Flood), "mDNS over IPv6");
        assert_eq!(to(segment::mac_for(3, 2)), Some(Route::Unicast(2)));
        assert_eq!(to(segment::mac_for(4, 2)), Some(Route::Flood), "another segment's MAC isn't known here");
        assert_eq!(to([0x02, 0, 0, 0, 0, 9]), Some(Route::Flood), "unknown unicast floods, as on a hub");
        assert_eq!(to(own), None, "not to itself");
        assert_eq!(route(&[0xff; 13], 3, 0), None, "shorter than an Ethernet header");
        assert_eq!(route(&[], 3, 0), None);
    }

    #[test]
    fn accepts_whole_frames_that_are_not_its_own() {
        let me = segment::mac_for(3, 0);
        assert!(acceptable(&frame(MDNS_V4, segment::mac_for(3, 1), "x"), me));
        assert!(!acceptable(&frame(MDNS_V4, me, "x"), me), "its own frame come back round");
        assert!(!acceptable(&[0u8; 13], me));
        assert!(!acceptable(&vec![0u8; MAX_FRAME + 1], me));
        assert!(acceptable(&frame(MDNS_V4, segment::mac_for(3, 1), &"x".repeat(1500)), me));
    }

    #[test]
    fn peers_are_the_other_sockets_in_the_listing() {
        let names = ["m2.sock", "m0.sock", "m1.sock", "segment.json", "m16.sock", "m2.sock", "m01.sock"];
        assert_eq!(peers_in(names, 1), [0, 2]);
        assert_eq!(peers_in([], 0), Vec::<u8>::new());
        assert_eq!(peers_in(["m0.sock"], 0), Vec::<u8>::new(), "alone");
    }

    #[test]
    fn multicast_reaches_every_other_member_and_not_the_sender() {
        let d = dir("mcast");
        let (mut a, mut b, mut c) = (join(&d, 0), join(&d, 1), join(&d, 2));
        let query = frame(MDNS_V4, a.mac(), "_nsd._tcp.local PTR?");
        a.port.transmit(&query);
        assert_eq!(b.next().as_deref(), Some(&query[..]));
        assert_eq!(c.next().as_deref(), Some(&query[..]));
        assert!(a.nothing_within(100), "the sender doesn't hear itself");
        assert!(b.nothing_within(50), "one copy each");
        assert_eq!(a.port.counter(|c| &c.sent), 2);
    }

    #[test]
    fn unicast_reaches_only_its_owner_and_answers_come_back() {
        let d = dir("ucast");
        let (mut a, mut b, mut c) = (join(&d, 0), join(&d, 1), join(&d, 2));
        let hello = frame(b.mac(), a.mac(), "10.77.3.1 -> 10.77.3.2");
        a.port.transmit(&hello);
        assert_eq!(b.next().as_deref(), Some(&hello[..]));
        assert!(c.nothing_within(100), "not a hub for known MACs");
        let reply = frame(a.mac(), b.mac(), "reply");
        b.port.transmit(&reply);
        assert_eq!(a.next().as_deref(), Some(&reply[..]));
    }

    #[test]
    fn an_unknown_unicast_floods() {
        let d = dir("unknown");
        let (mut a, mut b, mut c) = (join(&d, 0), join(&d, 1), join(&d, 2));
        let f = frame([0x02, 0, 0, 0, 0, 9], a.mac(), "who");
        a.port.transmit(&f);
        assert_eq!(b.next().as_deref(), Some(&f[..]));
        assert_eq!(c.next().as_deref(), Some(&f[..]));
    }

    #[test]
    fn a_frame_with_the_receivers_own_source_mac_is_dropped() {
        let d = dir("loop");
        let (mut a, mut b) = (join(&d, 0), join(&d, 1));
        // A frame that claims to come from b, sent by a: b must not take it.
        a.port.transmit(&frame(MDNS_V4, b.mac(), "spoof"));
        assert!(b.nothing_within(100));
        assert_eq!((a.port.counter(|c| &c.sent), b.port.counter(|c| &c.rx_dropped)), (1, 1), "sent, then refused on arrival");
    }

    #[test]
    fn a_frame_to_nobody_is_dropped_without_blocking_or_error() {
        let d = dir("nobody");
        let mut a = join(&d, 0);
        a.port.transmit(&frame(MDNS_V4, a.mac(), "alone"));
        a.port.transmit(&frame(segment::mac_for(3, 5), a.mac(), "unplugged"));
        assert_eq!(a.port.counter(|c| &c.sent), 0);
        assert_eq!(a.port.counter(|c| &c.tx_dropped), 1, "the unicast to an absent member");
        assert!(a.nothing_within(50));
    }

    #[test]
    fn a_member_that_starts_late_hears_later_frames() {
        let d = dir("late");
        let mut a = join(&d, 0);
        a.port.transmit(&frame(MDNS_V4, a.mac(), "early"));
        let mut b = join(&d, 1);
        let f = frame(MDNS_V4, a.mac(), "late");
        a.port.transmit(&f);
        assert_eq!(b.next().as_deref(), Some(&f[..]), "the peer list is read again");
        assert!(b.nothing_within(50), "and the early frame was not kept");
    }

    #[test]
    fn a_member_ringing_the_bell_wakes_its_device() {
        let d = dir("bell");
        let (mut a, mut b) = (join(&d, 0), join(&d, 1));
        let f = frame(MDNS_V4, a.mac(), "ring");
        a.port.transmit(&f);
        // The device's worker waits on the bell; a delivered frame must ring it.
        assert!(b.bell.wait(), "rung, not stopped");
        assert_eq!(b.port.receive().as_deref(), Some(&f[..]));
    }

    #[test]
    fn a_full_queue_is_bounded_never_blocks_the_sender_and_counts_what_it_drops() {
        let d = dir("full");
        let (mut a, b) = (join(&d, 0), join(&d, 1));
        let total = TO_GUEST + 40;
        let held = |b: &Member| lock(&b.port.shared.to_guest).len() as u64;
        let seen = |b: &Member| held(b) + b.port.counter(|c| &c.rx_dropped);
        for i in 0..total {
            // Nobody takes frames, so the guest's queue fills; pace the sender so the socket's own
            // buffer isn't what overflows first (a loaded host may still drop a send: counted).
            let ok = a.port.counter(|c| &c.sent);
            let until = Instant::now() + Duration::from_millis(500);
            while ok.saturating_sub(seen(&b)) > 16 && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(1));
            }
            a.port.transmit(&frame(MDNS_V4, a.mac(), &format!("{i}")));
        }
        // Whatever the sender lost to a full socket (`t`), every other frame is either held or
        // counted as dropped by the full queue, and the queue never grows past its bound.
        let t = a.port.counter(|c| &c.tx_dropped);
        let delivered = total as u64 - t;
        let until = Instant::now() + Duration::from_secs(5);
        while seen(&b) < delivered && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(a.port.counter(|c| &c.sent), delivered, "every send is counted once, as sent or dropped");
        assert_eq!(held(&b), delivered.min(TO_GUEST as u64));
        assert_eq!(b.port.counter(|c| &c.rx_dropped), delivered.saturating_sub(TO_GUEST as u64));
        if t == 0 {
            let first = lock(&b.port.shared.to_guest).pop_front().expect("oldest kept");
            assert!(first.ends_with(b"0"), "the oldest frame is the one kept");
        }
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_refused() {
        let d = dir("stale");
        std::fs::create_dir_all(&d).expect("dir");
        // A member that crashed: its socket file stays, nothing answers on it.
        drop(std::os::unix::net::UnixDatagram::bind(d.join("m1.sock")).expect("bind"));
        assert!(d.join("m1.sock").exists());
        drop(SegmentPort::open(&spec(&d, 3, 1)).expect("a stale socket is replaced"));
        // A running member can't be joined twice.
        let _running = join(&d, 0);
        let err = SegmentPort::open(&spec(&d, 3, 0)).err().expect("refused");
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
        assert!(err.to_string().contains("already running"), "{err}");
    }

    #[test]
    fn leaving_removes_the_socket_and_the_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("leave");
        let a = join(&d, 0);
        assert!(has_members(&d));
        assert_eq!(std::fs::metadata(&d).expect("dir").permissions().mode() & 0o777, 0o700);
        drop(a);
        assert!(!has_members(&d), "a member that stopped looks like one that isn't running");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_directory_is_judged_by_type_owner_and_mode() {
        use DirCheck::*;
        assert_eq!(judge_dir(true, 501, 0o40700, 501), Fine);
        assert_eq!(judge_dir(true, 501, 0o40500, 501), Fine, "tighter than 0700 is still private");
        assert_eq!(judge_dir(true, 501, 0o40755, 501), Tighten);
        assert_eq!(judge_dir(true, 501, 0o40777, 501), Tighten);
        assert_eq!(judge_dir(true, 501, 0o40710, 501), Tighten, "group access counts");
        assert!(matches!(judge_dir(true, 502, 0o40700, 501), Refuse(w) if w.contains("uid 502")), "someone else's, even at 0700");
        assert!(matches!(judge_dir(true, 0, 0o40755, 501), Refuse(_)), "root's too");
        assert!(matches!(judge_dir(false, 501, 0o100600, 501), Refuse(_)), "a file, or a link");
    }

    #[test]
    fn an_existing_directory_open_to_others_is_taken_back_and_a_link_or_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        let d = dir("shared");
        std::fs::create_dir_all(&d).expect("dir");
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let a = join(&d, 0);
        assert_eq!(mode(&d), 0o700, "ours, so tightened");
        drop(a);

        // A link to a directory (another user's, say) is not followed.
        let target = dir("shared-target");
        std::fs::create_dir_all(&target).expect("target");
        let link = dir("shared-link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let err = SegmentPort::open(&spec(&link, 3, 0)).err().expect("refused");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("not a directory"), "{err}");
        assert!(!has_members(&target), "nothing was bound through the link");

        // A file where the directory should be.
        let file = dir("shared-file");
        std::fs::write(&file, b"x").expect("file");
        assert!(SegmentPort::open(&spec(&file, 3, 0)).is_err());
        for p in [&link, &file] {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn an_invalid_spec_is_refused_before_touching_the_disk() {
        let d = dir("invalid");
        let err = SegmentPort::open(&spec(&d, 0, 0)).err().expect("refused");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(!d.exists());
    }

    #[test]
    fn an_idle_segment_wakes_its_thread_for_nothing() {
        let d = dir("idle");
        let a = join(&d, 0);
        std::thread::sleep(Duration::from_millis(300));
        // One wakeup is the poll returning once at start-up at most; an idle member polls forever.
        assert!(a.port.counter(|c| &c.wakeups) <= 1, "{}", a.port.counter(|c| &c.wakeups));
        let b = join(&d, 1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(a.port.counter(|c| &c.wakeups) <= 1 && b.port.counter(|c| &c.wakeups) <= 1);
        assert_eq!(a.port.counter(|c| &c.sent), 0);
    }
}
