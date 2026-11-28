//! A user-mode NAT behind virtio-net: the guest's internet, as stock's slirp gives it.
//!
//! The guest sits on 10.0.2.0/24 at 10.0.2.15 (the radio HAL's `+CGCONTRDP` config). The NAT is
//! its gateway, 10.0.2.2, and its resolver, 10.0.2.3, and answers ARP for those two only. Every
//! flow the guest opens becomes a host socket of the emu process, so it needs no root, no
//! entitlement and no host interface:
//!
//! - TCP: a guest SYN starts a non-blocking connect to the real destination; once that
//!   connects, a smoltcp socket answers the SYN (a refused or failed connect answers with RST).
//!   Data moves between smoltcp's buffers and the host socket with no copy in between.
//! - UDP: one connected host socket per guest (source, destination) pair, dropped when idle.
//! - ICMP echo: one unprivileged ICMP datagram socket per (destination, identifier); echoes to
//!   the NAT's own addresses are answered here.
//! - 10.0.2.2 is the host's loopback, as on stock; 10.0.2.3 is the host's resolver
//!   (`/etc/resolv.conf`).
//!
//! One thread per device runs it, blocked in `poll(2)` over the host sockets it is waiting on
//! and a wakeup the device rings when the guest sends; its timeout is smoltcp's next timer or
//! the next idle flow's expiry. An idle device costs no CPU.
//!
//! The engine is POSIX throughout (`poll(2)` over raw descriptors, a socket pair for the wakeup),
//! so it exists on unix only. Elsewhere (Windows) [`Nat::new`] fails with
//! [`io::ErrorKind::Unsupported`] and the device runs link only, as it does when the NAT can't
//! start.
#[cfg(unix)]
mod engine;
#[cfg(unix)]
mod host;
// Pure frame and shaping logic: built everywhere (and tested), used by the unix-only engine.
#[cfg_attr(not(unix), allow(dead_code))]
mod packet;
#[cfg_attr(not(unix), allow(dead_code))]
mod shape;
#[cfg(all(test, unix))]
mod tests;

#[cfg(unix)]
use std::collections::VecDeque;
use std::io;
#[cfg(unix)]
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::sync::MutexGuard;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::thread::JoinHandle;

use crate::virtio::net::{Backend, RxReady};

/// slirp's MAC for its virtual hosts (`52:55:0a:00:02:02`), which the guest learns for 10.0.2.2.
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

/// Where DNS to the guest's resolver address goes.
#[derive(Clone, Debug)]
pub enum Dns {
    /// The host's resolver, from `/etc/resolv.conf` (which macOS keeps current), re-read when
    /// the host's network may have changed.
    System,
    Fixed(SocketAddr),
}

/// Frames held for the guest. Past this smoltcp holds its segments (and retries) instead.
#[cfg(unix)]
const TO_GUEST: usize = 256;
/// Frames from the guest waiting for the net thread; past this they drop, as on a full link.
#[cfg(unix)]
const TO_HOST: usize = 1024;

/// The guest's network, as stock lays it out.
#[derive(Clone, Debug)]
pub struct Config {
    pub gateway: Ipv4Addr,
    pub resolver: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway_mac: [u8; 6],
    /// What the gateway's address reaches on the host.
    pub loopback: IpAddr,
    pub dns: Dns,
    /// How many flows (each a host socket) the guest may hold open.
    pub limits: Limits,
    /// The console's `network speed|delay`: read live, so a change applies to the next frame.
    pub network: Arc<Mutex<sim_core::network::Conditions>>,
}

/// Caps on the guest's flows. The guest is untrusted and every flow is a host file descriptor
/// of the emu process: past the process's limit (`RLIMIT_NOFILE`, 256 by default on macOS) new
/// flows fail and so does every `accept` emu makes (the console, the adb bridge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// TCP connections, connecting or open; a SYN past this is reset.
    pub tcp: usize,
    /// Of those, host connects still in progress.
    pub tcp_connecting: usize,
    /// UDP flows; past this the one closest to expiring is dropped.
    pub udp: usize,
    /// ICMP echo flows, likewise.
    pub ping: usize,
}

/// File descriptors left for the rest of emu: disks, the console and its clients, the adb
/// bridge, the display socket and frame rings, the renderer, the NAT's own wakeup.
const RESERVED_FDS: u64 = 128;

impl Limits {
    /// The caps for a process allowed `fds` descriptors: at most 1024 TCP (256 connecting),
    /// 512 UDP and 64 ICMP flows, fewer when the limit leaves less than that after
    /// [`RESERVED_FDS`].
    pub fn for_fds(fds: u64) -> Limits {
        let budget = usize::try_from(fds.saturating_sub(RESERVED_FDS)).unwrap_or(usize::MAX);
        let udp = (budget / 4).min(512);
        let ping = (budget / 16).min(64);
        let tcp = (budget - udp - ping).clamp(1, 1024);
        Limits { tcp, tcp_connecting: tcp.min(256), udp: udp.max(1), ping: ping.max(1) }
    }

    /// The caps for this process's current `RLIMIT_NOFILE` (emu raises it at start).
    pub fn from_rlimit() -> Limits {
        #[cfg(unix)]
        let fds = rustix::process::getrlimit(rustix::process::Resource::Nofile).current.unwrap_or(u64::MAX);
        // No descriptor limit to read: sockets there are handles, capped by memory.
        #[cfg(not(unix))]
        let fds = u64::MAX;
        Limits::for_fds(fds)
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            gateway: Ipv4Addr::new(10, 0, 2, 2),
            resolver: Ipv4Addr::new(10, 0, 2, 3),
            prefix_len: 24,
            gateway_mac: GATEWAY_MAC,
            loopback: IpAddr::V4(Ipv4Addr::LOCALHOST),
            dns: Dns::System,
            limits: Limits::from_rlimit(),
            network: Arc::default(),
        }
    }
}

/// What the device's worker and the net thread share.
#[cfg(unix)]
struct Shared {
    to_host: Mutex<VecDeque<Vec<u8>>>,
    to_guest: Mutex<VecDeque<Vec<u8>>>,
    /// While the upload is shaped: the most bytes the guest's frames may wait for the net
    /// thread (about a second of the rate; `usize::MAX` otherwise), and how many wait now, in
    /// `to_host` and in the thread's hands. Past the cap frames drop, as at a full link.
    up_cap: AtomicUsize,
    up_held: AtomicUsize,
    /// Whether the thread holds a shaper (for tests: unshaped, none exists).
    shaped: AtomicBool,
    /// Whether the engine holds a shaper at all, as the engine sees it (for tests).
    #[cfg(test)]
    shaper_built: AtomicBool,
    ready: Mutex<Option<RxReady>>,
    /// The net thread stopped taking smoltcp's frames because `to_guest` was full: the worker
    /// wakes it once the guest has taken half.
    guest_full: AtomicBool,
    stop: AtomicBool,
    reset: AtomicBool,
    wake: UnixStream,
    /// Loop iterations and open flows, for tests and diagnostics.
    iterations: AtomicU64,
    flows: AtomicUsize,
}

#[cfg(unix)]
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(unix)]
impl Shared {
    fn wake(&self) {
        // A full socket buffer means a wakeup is already pending.
        let _ = (&self.wake).write(&[1]);
    }

    /// Sets the upload's cap (`held`: the bytes the thread holds already). Under the queue's
    /// lock, so frames the device adds are counted once.
    fn set_up_cap(&self, cap: Option<usize>, held: usize) {
        let q = lock(&self.to_host);
        let queued: usize = q.iter().map(Vec::len).sum();
        self.up_held.store(queued + held, Ordering::Relaxed);
        self.up_cap.store(cap.unwrap_or(usize::MAX), Ordering::Relaxed);
    }

    /// The net thread took a frame of `bytes` off the upload's queue (shaped only).
    fn up_released(&self, bytes: usize) {
        let _ = self.up_held.try_update(Ordering::Relaxed, Ordering::Relaxed, |h| Some(h.saturating_sub(bytes)));
    }

    fn ring_guest(&self) {
        if let Some(r) = lock(&self.ready).as_ref() {
            r.ring();
        }
    }
}

/// The NAT, as the virtio-net device's [`Backend`]. Its thread starts on the device's first
/// activation; a later activation (the guest rebooted or reset the device) drops every flow.
#[cfg(unix)]
pub struct Nat {
    shared: Arc<Shared>,
    /// The engine's wakeup end and config, until the thread takes them.
    start: Option<(UnixStream, Config)>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl Nat {
    pub fn new(cfg: Config) -> io::Result<Nat> {
        let (wake, wait) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wait.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            to_host: Mutex::new(VecDeque::new()),
            to_guest: Mutex::new(VecDeque::new()),
            up_cap: AtomicUsize::new(usize::MAX),
            up_held: AtomicUsize::new(0),
            shaped: AtomicBool::new(false),
            #[cfg(test)]
            shaper_built: AtomicBool::new(false),
            ready: Mutex::new(None),
            guest_full: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            wake,
            iterations: AtomicU64::new(0),
            flows: AtomicUsize::new(0),
        });
        Ok(Nat { shared, start: Some((wait, cfg)), thread: None })
    }

    #[cfg(test)]
    fn iterations(&self) -> u64 {
        self.shared.iterations.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn shaped(&self) -> bool {
        self.shared.shaped.load(Ordering::Relaxed)
    }

    /// Whether the engine holds a shaper (not whether the conditions ask for one).
    #[cfg(test)]
    fn shaper_built(&self) -> bool {
        self.shared.shaper_built.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn flows(&self) -> usize {
        self.shared.flows.load(Ordering::Relaxed)
    }
}

#[cfg(unix)]
impl Backend for Nat {
    fn attach(&mut self, ready: RxReady) {
        *lock(&self.shared.ready) = Some(ready);
        lock(&self.shared.to_guest).clear();
        lock(&self.shared.to_host).clear();
        self.shared.up_held.store(0, Ordering::Relaxed);
        if let Some((wait, cfg)) = self.start.take() {
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new().name("virtio-net-nat".into()).spawn(move || {
                crate::device_thread();
                engine::Engine::new(cfg, shared).run(wait);
            });
            match spawned {
                Ok(h) => self.thread = Some(h),
                Err(e) => eprintln!("emu: net: no NAT thread ({e}); the network is link only"),
            }
        } else {
            self.shared.reset.store(true, Ordering::Release);
            self.shared.wake();
        }
    }

    fn transmit(&mut self, frame: &[u8]) {
        if self.thread.is_none() {
            return;
        }
        let mut q = lock(&self.shared.to_host);
        if q.len() >= TO_HOST {
            return;
        }
        // Counted only while shaped (the cap is finite): the unshaped path adds nothing.
        let cap = self.shared.up_cap.load(Ordering::Relaxed);
        if cap != usize::MAX {
            let held = self.shared.up_held.load(Ordering::Relaxed);
            if held + frame.len() > cap && held > 0 {
                return;
            }
            self.shared.up_held.fetch_add(frame.len(), Ordering::Relaxed);
        }
        q.push_back(frame.to_vec());
        let first = q.len() == 1;
        drop(q);
        if first {
            self.shared.wake();
        }
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        let mut q = lock(&self.shared.to_guest);
        let f = q.pop_front();
        if q.len() <= TO_GUEST / 2 && self.shared.guest_full.swap(false, Ordering::AcqRel) {
            drop(q);
            self.shared.wake();
        }
        f
    }
}

#[cfg(unix)]
impl Drop for Nat {
    fn drop(&mut self) {
        if let Some(h) = self.thread.take() {
            self.shared.stop.store(true, Ordering::Release);
            self.shared.wake();
            let _ = h.join();
        }
    }
}

/// No NAT where the engine doesn't exist: no value can be built, so the device gets
/// [`io::ErrorKind::Unsupported`] and falls back to link only.
#[cfg(not(unix))]
pub struct Nat(std::convert::Infallible);

#[cfg(not(unix))]
impl Nat {
    pub fn new(_cfg: Config) -> io::Result<Nat> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the user-mode NAT is not available on this host yet; the network is link only"))
    }
}

#[cfg(not(unix))]
impl Backend for Nat {
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

#[cfg(all(test, not(unix)))]
mod unsupported_tests {
    use super::*;

    #[test]
    fn the_nat_says_it_is_unsupported() {
        assert_eq!(Nat::new(Config::default()).err().map(|e| e.kind()), Some(io::ErrorKind::Unsupported));
    }
}
