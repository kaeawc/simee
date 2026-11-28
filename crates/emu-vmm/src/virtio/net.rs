//! virtio-net at the stock board's net slot, so the guest has an `eth0`.
//!
//! The ranchu radio HAL configures `eth0` itself from the modem's `+CGCONTRDP` reply (address,
//! gateway, DNS; no DHCP) and fails every data call while the interface is missing. The device
//! moves Ethernet frames between the guest and a [`Backend`]: the user-mode NAT (`crate::net`),
//! or [`LinkOnly`] (the link comes up, nothing gets through) when the NAT can't start.
//!
//! Features are the minimum Linux's driver binds with: a MAC, the link status, event-idx. No
//! mergeable rx buffers, checksum offload or GSO, so every rx buffer holds one whole frame and
//! every tx chain is one frame.
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread::JoinHandle;

use emu_hvf::GuestRam;

use super::mmio::F_RING_EVENT_IDX;
use super::queue::{Chain, Queue};
use super::{Activation, Doorbell, ID_NET, INT_USED_RING, Interrupt, VirtioDevice};

pub const F_MAC: u64 = 1 << 5;
pub const F_STATUS: u64 = 1 << 16;
pub const S_LINK_UP: u16 = 1;

/// Stock's MAC for an AVD's first NIC (QEMU's default, `52:54:00:12:34:56`).
pub const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// `virtio_net_hdr_v1`: with VERSION_1 it always carries `num_buffers` (offset 10).
pub const HDR: usize = 12;
/// The largest tx frame taken: without GSO the guest sends at most its MTU plus the Ethernet
/// header; this leaves room for any MTU it sets.
const MAX_FRAME: usize = 65_535;
/// Frames held for the guest while it has no rx buffer posted; the oldest go first past this.
const RX_BACKLOG: usize = 64;

const RXQ: usize = 0;
const TXQ: usize = 1;

/// Wakes the device's worker to collect frames from its backend: a backend whose frames arrive
/// on another thread rings it after queueing one.
#[derive(Clone)]
pub struct RxReady(Arc<Doorbell>);

impl RxReady {
    pub fn new(bell: Arc<Doorbell>) -> RxReady {
        RxReady(bell)
    }

    pub fn ring(&self) {
        self.0.ring();
    }
}

/// Where the guest's frames go and the frames for it come from (whole Ethernet frames, no
/// virtio header). Runs on the device's worker thread.
pub trait Backend: Send {
    /// Called on each activation, before any frame moves, with the wakeup for that worker.
    fn attach(&mut self, _ready: RxReady) {}
    /// A frame the guest sent.
    fn transmit(&mut self, frame: &[u8]);
    /// The next frame for the guest, if one is waiting.
    fn receive(&mut self) -> Option<Vec<u8>>;
}

/// The link is up and goes nowhere: transmitted frames are dropped and none arrive.
pub struct LinkOnly;

impl Backend for LinkOnly {
    fn transmit(&mut self, _frame: &[u8]) {}
    fn receive(&mut self) -> Option<Vec<u8>> {
        None
    }
}

pub struct Net {
    mac: [u8; 6],
    /// Here while no worker runs; the worker hands it back when it stops.
    backend: Option<Box<dyn Backend>>,
    worker: Option<(Arc<Doorbell>, WorkerHandle)>,
}

/// The worker thread, which returns the backend when it stops.
type WorkerHandle = JoinHandle<Box<dyn Backend>>;

impl Net {
    pub fn new(backend: Box<dyn Backend>) -> Net {
        Net::with_mac(DEFAULT_MAC, backend)
    }

    /// A NIC with its own MAC: the segment NIC's, which differs from the NAT NIC's.
    pub fn with_mac(mac: [u8; 6], backend: Box<dyn Backend>) -> Net {
        Net { mac, backend: Some(backend), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some((bell, handle)) = self.worker.take() {
            bell.stop();
            if let Ok(b) = handle.join() {
                self.backend = Some(b);
            }
        }
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Net {
    fn device_id(&self) -> u32 {
        ID_NET
    }
    fn features(&self) -> u64 {
        F_MAC | F_STATUS | F_RING_EVENT_IDX
    }
    fn num_queues(&self) -> usize {
        2
    }
    /// `virtio_net_config`: mac[6], then status (le16). The fields after it need features this
    /// device doesn't offer.
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 8];
        cfg[..6].copy_from_slice(&self.mac);
        cfg[6..8].copy_from_slice(&S_LINK_UP.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let rxq = act.queues.get(RXQ).copied().flatten().ok_or("net rx queue not enabled")?;
        let txq = act.queues.get(TXQ).copied().flatten().ok_or("net tx queue not enabled")?;
        self.stop_worker();
        let mut backend = self.backend.take().ok_or("net backend lost")?;
        let bell = Arc::new(Doorbell::default());
        backend.attach(RxReady(bell.clone()));
        let event_idx = act.features & F_RING_EVENT_IDX != 0;
        let mut w = Worker {
            mem: act.mem,
            irq: act.irq,
            rxq: Queue::new(rxq, event_idx),
            txq: Queue::new(txq, event_idx),
            backend,
            pending: VecDeque::new(),
            tx_chain: Chain::default(),
            tx_buf: Vec::new(),
            irq_due: false,
        };
        let b = bell.clone();
        let handle = std::thread::Builder::new()
            .name("virtio-net".into())
            .spawn(move || {
                crate::device_thread();
                while b.wait() {
                    w.pass();
                }
                w.backend
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some((bell, handle));
        Ok(())
    }
    fn notify(&mut self, _queue: u16) {
        if let Some((bell, _)) = &self.worker {
            bell.ring();
        }
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    rxq: Queue,
    txq: Queue,
    backend: Box<dyn Backend>,
    /// Frames for the guest waiting for an rx buffer.
    pending: VecDeque<Vec<u8>>,
    tx_chain: Chain,
    tx_buf: Vec<u8>,
    /// Used entries the driver wants an interrupt for: raised once per pass.
    irq_due: bool,
}

impl Worker {
    /// One wakeup: everything the guest sent, then everything for it that fits. Frames are
    /// taken from the backend only while there is room to hold them, so a backend that bounds
    /// its own queue (the NAT) sees the guest's pace instead of losing frames here.
    fn pass(&mut self) {
        self.drain_tx();
        loop {
            let mut drained = false;
            while self.pending.len() < RX_BACKLOG {
                match self.backend.receive() {
                    Some(f) => self.pending.push_back(f),
                    None => {
                        drained = true;
                        break;
                    }
                }
            }
            self.fill_rx();
            // Out of rx buffers (the guest's refill kick comes back here) or out of frames.
            if !self.pending.is_empty() || drained {
                break;
            }
        }
        if std::mem::take(&mut self.irq_due) {
            self.irq.raise(INT_USED_RING);
        }
    }

    fn drain_tx(&mut self) {
        let mut used = false;
        loop {
            match self.txq.pop_draining(&self.mem, &mut self.tx_chain) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    eprintln!("emu: virtio-net tx: {e}");
                    break;
                }
            }
            match self.tx_chain.read_all_into(&self.mem, HDR + MAX_FRAME, &mut self.tx_buf) {
                Ok(()) if self.tx_buf.len() > HDR => self.backend.transmit(&self.tx_buf[HDR..]),
                Ok(()) => {}
                Err(e) => eprintln!("emu: virtio-net tx: {e}"),
            }
            if let Err(e) = self.txq.add_used(&self.mem, self.tx_chain.head, 0) {
                eprintln!("emu: virtio-net tx: {e}");
            }
            used = true;
        }
        if used && self.txq.should_notify(&self.mem) {
            self.irq_due = true;
        }
    }

    /// Puts pending frames in rx buffers until either runs out. The queue asks for the guest's
    /// refill kicks only while starved (as vsock's rx does): then it rechecks after asking, so
    /// a buffer posted in between is used now and one posted after it kicks.
    fn fill_rx(&mut self) {
        let mut used = false;
        while let Some(frame) = self.pending.front() {
            self.rxq.disable_notify(&self.mem);
            let chain = loop {
                match self.rxq.pop(&self.mem) {
                    Ok(Some(c)) => break Some(c),
                    Ok(None) if self.rxq.enable_notify(&self.mem) => self.rxq.disable_notify(&self.mem),
                    Ok(None) => break None,
                    Err(e) => {
                        eprintln!("emu: virtio-net rx: {e}");
                        self.rxq.enable_notify(&self.mem);
                        break None;
                    }
                }
            };
            let Some(chain) = chain else { break };
            let room: usize = chain.writable().map(|d| d.len as usize).sum();
            let mut hdr = [0u8; HDR];
            hdr[10..12].copy_from_slice(&1u16.to_le_bytes()); // num_buffers
            // A frame that doesn't fit is dropped rather than cut short; the buffer goes back
            // empty (the driver drops anything shorter than a header).
            let written = if room >= HDR + frame.len() { chain.write_parts(&self.mem, &[&hdr, frame]).unwrap_or(0) } else { 0 };
            if let Err(e) = self.rxq.add_used(&self.mem, chain.head, written) {
                eprintln!("emu: virtio-net rx: {e}");
            }
            self.pending.pop_front();
            used = true;
        }
        if used && self.rxq.should_notify(&self.mem) {
            self.irq_due = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::bus::MmioDevice;
    use crate::irq::TestLine;
    use crate::virtio::mmio::{F_VERSION_1, MmioTransport};
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    const RING: u16 = 8;
    const BUFS: u64 = RAM_BASE + 0x10_000;
    const FEATURES: u64 = F_MAC | F_STATUS | F_RING_EVENT_IDX | F_VERSION_1;

    /// What the test backend saw, and the frames it has for the guest.
    #[derive(Default)]
    struct Wire {
        sent: Vec<Vec<u8>>,
        inbound: VecDeque<Vec<u8>>,
        ready: Option<RxReady>,
        attached: u32,
    }

    struct TestBackend(Arc<Mutex<Wire>>);

    impl Backend for TestBackend {
        fn attach(&mut self, ready: RxReady) {
            let mut w = self.0.lock().expect("wire");
            w.ready = Some(ready);
            w.attached += 1;
        }
        fn transmit(&mut self, frame: &[u8]) {
            self.0.lock().expect("wire").sent.push(frame.to_vec());
        }
        fn receive(&mut self) -> Option<Vec<u8>> {
            self.0.lock().expect("wire").inbound.pop_front()
        }
    }

    impl Wire {
        /// Queues a frame for the guest and wakes the worker, as a backend's own thread would.
        fn deliver(wire: &Mutex<Wire>, frame: &[u8]) {
            let ready = {
                let mut w = wire.lock().expect("wire");
                w.inbound.push_back(frame.to_vec());
                w.ready.clone().expect("attached")
            };
            ready.ring();
        }
    }

    struct Guest {
        mem: Arc<GuestRam>,
        dev: Net,
        wire: Arc<Mutex<Wire>>,
        line: Arc<TestLine>,
        rx: Driver,
        tx: Driver,
        posted: u16,
    }

    fn guest() -> Guest {
        let wire = Arc::new(Mutex::new(Wire::default()));
        let mut g = Guest {
            mem: Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram")),
            dev: Net::new(Box::new(TestBackend(wire.clone()))),
            wire,
            line: Arc::new(TestLine::default()),
            rx: Driver::at(RAM_BASE + 0x1000, RING),
            tx: Driver::at(RAM_BASE + 0x3000, RING),
            posted: 0,
        };
        g.activate();
        g
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    impl Guest {
        fn activate(&mut self) {
            let queues = vec![Some(self.rx.cfg), Some(self.tx.cfg)];
            let irq = Arc::new(Interrupt::new(self.line.clone()));
            self.dev.activate(Activation { mem: self.mem.clone(), irq, queues, features: FEATURES }).expect("activate");
        }

        fn rx_buf(i: u16) -> u64 {
            BUFS + u64::from(i % RING) * 0x800
        }

        /// Posts one rx buffer of `len` bytes; kicks if the device asked for it.
        fn post_rx(&mut self, len: u32) -> bool {
            let old = self.rx.avail_idx();
            self.rx.push(&self.mem, &[(Self::rx_buf(self.posted), len, true)]);
            self.posted += 1;
            let kick = self.rx.needs_kick(&self.mem, old, true);
            if kick {
                self.dev.notify(RXQ as u16);
            }
            kick
        }

        /// Sends one frame as Linux does without offloads: a zeroed header and the frame, here
        /// split across two descriptors.
        fn send(&mut self, frame: &[u8]) {
            let at = RAM_BASE + 0x40_000 + u64::from(self.tx.avail_idx() % RING) * 0x1000;
            self.mem.write(at, &[0u8; HDR]).expect("hdr");
            self.mem.write(at + 0x100, frame).expect("frame");
            let old = self.tx.avail_idx();
            self.tx.push(&self.mem, &[(at, HDR as u32, false), (at + 0x100, frame.len() as u32, false)]);
            if self.tx.needs_kick(&self.mem, old, true) {
                self.dev.notify(TXQ as u16);
            }
        }

        /// The `i`th rx completion: (header, frame).
        fn received(&self, i: u16) -> ([u8; HDR], Vec<u8>) {
            let (id, len) = self.rx.used(&self.mem, i % RING);
            let mut b = vec![0u8; len as usize];
            self.mem.read(Self::rx_buf(id as u16), &mut b).expect("rx");
            let mut hdr = [0u8; HDR];
            hdr.copy_from_slice(&b[..HDR]);
            (hdr, b[HDR..].to_vec())
        }
    }

    fn frame(n: u8) -> Vec<u8> {
        let mut f = vec![0xff; 6]; // broadcast
        f.extend_from_slice(&DEFAULT_MAC);
        f.extend_from_slice(&[0x08, 0x06]); // ARP
        f.extend(std::iter::repeat_n(n, 46));
        f
    }

    #[test]
    fn a_second_nic_reports_its_own_mac() {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x4000).expect("ram"));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let mac = sim_core::segment::mac_for(3, 1);
        let t = MmioTransport::new(Some(Box::new(Net::with_mac(mac, Box::new(LinkOnly)))), irq, mem);
        let mut b = [0u8; 6];
        t.read(0x100, &mut b);
        assert_eq!(b, mac);
        assert_ne!(b, DEFAULT_MAC, "two NICs never share a MAC");
    }

    #[test]
    fn the_net_slot_identifies_with_a_mac_and_the_link_up() {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x4000).expect("ram"));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let t = MmioTransport::new(Some(Box::new(Net::new(Box::new(LinkOnly)))), irq, mem);
        let r = |offset: u64, len: usize| {
            let mut b = vec![0u8; len];
            t.read(offset, &mut b);
            b
        };
        let r32 = |offset: u64| u32::from_le_bytes(r(offset, 4).try_into().expect("4"));
        assert_eq!(r32(0x008), ID_NET);
        t.write(0x014, &0u32.to_le_bytes());
        assert_eq!(r32(0x010), (F_MAC | F_STATUS | F_RING_EVENT_IDX) as u32);
        t.write(0x014, &1u32.to_le_bytes());
        assert_eq!(r32(0x010), 1, "VERSION_1 and nothing else in the high word");
        t.write(0x030, &1u32.to_le_bytes());
        assert_ne!(r32(0x034), 0, "a tx queue");
        t.write(0x030, &2u32.to_le_bytes());
        assert_eq!(r32(0x034), 0, "no control queue");
        assert_eq!(r(0x100, 6), DEFAULT_MAC);
        assert_eq!(r(0x106, 2), S_LINK_UP.to_le_bytes());
        assert_eq!(r(0x103, 4), [0x12, 0x34, 0x56, 1], "any offset and length");
        assert_eq!(r(0x106, 4), [1, 0, 0, 0], "zero past the status");
        assert_eq!(r(0x110, 2), [0, 0]);
    }

    #[test]
    fn a_tx_frame_reaches_the_backend_without_its_header() {
        let mut g = guest();
        g.send(&frame(1));
        g.send(&frame(2));
        wait_for("tx completions", || g.tx.used_idx(&g.mem) == 2);
        assert_eq!(g.tx.used(&g.mem, 0), (0, 0), "returned, nothing written");
        assert_eq!(g.wire.lock().expect("wire").sent, [frame(1), frame(2)]);
        wait_for("the interrupt the driver asked for", || g.line.level());
    }

    #[test]
    fn rx_frames_fill_posted_buffers_and_wait_for_one_when_none_is() {
        let mut g = guest();
        g.post_rx(0x800);
        Wire::deliver(&g.wire, &frame(7));
        wait_for("rx completion", || g.rx.used_idx(&g.mem) == 1);
        let (hdr, got) = g.received(0);
        assert_eq!(hdr, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0], "num_buffers 1");
        assert_eq!(got, frame(7));
        wait_for("rx interrupt", || g.line.level());

        // No buffer left: the frame waits, and the device asks for the next refill's kick.
        Wire::deliver(&g.wire, &frame(8));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(g.rx.used_idx(&g.mem), 1, "nowhere to put it");
        assert!(g.post_rx(0x800), "a starved device wants the refill kick");
        wait_for("held frame delivered", || g.rx.used_idx(&g.mem) == 2);
        assert_eq!(g.received(1).1, frame(8));
    }

    #[test]
    fn a_frame_too_big_for_the_buffer_is_dropped_not_cut() {
        let mut g = guest();
        g.post_rx(32);
        g.post_rx(0x800);
        Wire::deliver(&g.wire, &frame(1));
        Wire::deliver(&g.wire, &frame(2));
        wait_for("both buffers back", || g.rx.used_idx(&g.mem) == 2);
        assert_eq!(g.rx.used(&g.mem, 0).1, 0, "the small buffer returns empty");
        assert_eq!(g.received(1).1, frame(2));
    }

    #[test]
    fn reset_joins_the_worker_and_a_second_activation_works() {
        let mut g = guest();
        g.dev.reset();
        assert!(g.dev.worker.is_none() && g.dev.backend.is_some(), "the worker stopped and handed its backend back");
        g.dev.notify(TXQ as u16); // reset: nothing to wake
        g.rx = Driver::at(RAM_BASE + 0x1000, RING);
        g.tx = Driver::at(RAM_BASE + 0x3000, RING);
        g.mem.write(RAM_BASE + 0x1000, &[0u8; 0x4000]).expect("clear rings");
        g.posted = 0;
        g.activate();
        assert_eq!(g.wire.lock().expect("wire").attached, 2, "attached once per activation");
        g.send(&frame(3));
        wait_for("tx after re-activation", || g.tx.used_idx(&g.mem) == 1);
        assert_eq!(g.wire.lock().expect("wire").sent, [frame(3)]);
    }

    #[test]
    fn frames_past_the_backlog_wait_in_the_backend_instead_of_dropping() {
        let mut g = guest();
        let n = RX_BACKLOG as u16 + 40;
        {
            let mut w = g.wire.lock().expect("wire");
            for i in 0..n {
                w.inbound.push_back(frame(i as u8));
            }
        }
        g.wire.lock().expect("wire").ready.clone().expect("attached").ring();
        let mut got = 0;
        while got < n {
            for _ in 0..RING {
                g.post_rx(0x800);
            }
            wait_for("a ring of frames", || g.rx.used_idx(&g.mem) == got + RING.min(n - got));
            for i in got..g.rx.used_idx(&g.mem) {
                assert_eq!(g.received(i).1, frame(i as u8), "frame {i} in order, none dropped");
            }
            got = g.rx.used_idx(&g.mem);
        }
    }

    #[test]
    fn link_only_drops_everything() {
        let mut b = LinkOnly;
        b.transmit(&frame(1));
        assert!(b.receive().is_none());
    }
}
