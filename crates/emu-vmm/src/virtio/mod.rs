//! virtio devices over the MMIO transport.
pub mod blk;
pub mod console;
// Wherever the renderer loads: emu's own machine on Apple silicon, and vhost-user on Linux.
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), target_os = "linux"))]
pub mod gpu;
pub mod input;
pub mod legacy_asg;
pub mod mmio;
pub mod net;
pub mod queue;
pub mod rng;
pub mod snd;
pub mod vsock;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use emu_hvf::GuestRam;

use crate::irq::IrqLine;
use queue::QueueConfig;

pub const ID_NET: u32 = 1;
pub const ID_BLOCK: u32 = 2;
pub const ID_RNG: u32 = 4;

/// What a device receives when the driver sets DRIVER_OK.
pub struct Activation {
    pub mem: Arc<GuestRam>,
    pub irq: Arc<Interrupt>,
    /// One entry per queue; `None` for queues the driver left disabled.
    pub queues: Vec<Option<QueueConfig>>,
    pub features: u64,
}

/// A virtio device model. The transport owns it behind a lock; devices that do real work move
/// their queues to a worker thread in `activate` and have `notify` wake it.
pub trait VirtioDevice: Send {
    fn device_id(&self) -> u32;
    /// Device feature bits (the transport adds VIRTIO_F_VERSION_1).
    fn features(&self) -> u64;
    fn num_queues(&self) -> usize;
    fn queue_max_size(&self) -> u16 {
        256
    }
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(&mut self, act: Activation) -> Result<(), String>;
    fn notify(&mut self, queue: u16);
    /// Driver reset: stop workers and forget queue state.
    fn reset(&mut self);
    /// Shared memory region `id` as (guest physical base, length), if the device has one.
    fn shm_region(&self, _id: u8) -> Option<(u64, u64)> {
        None
    }
}

/// The transport's interrupt status register and the level-triggered line it drives.
///
/// The device tree marks virtio-mmio interrupts level-high: the line stays asserted while any
/// status bit is set, so an interrupt raised while the driver is mid-handler is never lost.
///
/// The status is one atomic word (as in crosvm, cloud-hypervisor and Firecracker): reading it
/// (the driver's ISR read) and a raise or ack that leaves it zero or non-zero as it was take no
/// lock and never reach the controller. Those VMMs signal an edge (irqfd) and never lower a
/// line; this one is level, so the two edges (0 -> set, set -> 0) re-sync the line under a lock
/// to whatever the status is *then*. Zero-ness only changes at an edge and every edge syncs
/// after its own update, so the last sync sees the final status: a raise racing an ack can
/// neither leave a bit set with the line down nor the line up with nothing set.
pub struct Interrupt {
    status: AtomicU32,
    /// The level last driven on the line; held while driving it.
    level: Mutex<bool>,
    line: Arc<dyn IrqLine>,
    /// Every used-ring raise signals the line and nothing is held ([`Interrupt::edge`]).
    edge: bool,
}

pub const INT_USED_RING: u32 = 1;
pub const INT_CONFIG: u32 = 2;

impl Interrupt {
    pub fn new(line: Arc<dyn IrqLine>) -> Interrupt {
        Interrupt { status: AtomicU32::new(0), level: Mutex::new(false), line, edge: false }
    }

    /// For a transport whose interrupt is a message with no status for the driver to read and
    /// acknowledge (a vhost-user call eventfd, which the frontend turns into the queue's MSI):
    /// every used-ring raise signals the line, which coalesces signals itself, and no status is
    /// kept, since nothing would clear it. Config-change raises are dropped: they'd need a
    /// backend message, and no device served this way changes its config.
    pub fn edge(line: Arc<dyn IrqLine>) -> Interrupt {
        Interrupt { status: AtomicU32::new(0), level: Mutex::new(false), line, edge: true }
    }

    pub fn status(&self) -> u32 {
        self.status.load(Ordering::Acquire)
    }

    pub fn raise(&self, bits: u32) {
        if self.edge {
            if bits & INT_USED_RING != 0 {
                self.line.set(true);
            }
            return;
        }
        let was = self.status.fetch_or(bits, Ordering::AcqRel);
        if was == 0 && bits != 0 {
            self.sync_line();
        }
    }

    pub fn ack(&self, bits: u32) {
        let was = self.status.fetch_and(!bits, Ordering::AcqRel);
        if was != 0 && was & !bits == 0 {
            self.sync_line();
        }
    }

    fn sync_line(&self) {
        let mut level = self.level.lock().unwrap_or_else(|p| p.into_inner());
        let want = self.status.load(Ordering::Acquire) != 0;
        if *level != want {
            *level = want;
            self.line.set(want);
        }
    }
}

/// Wakeup for a device worker thread: notifications coalesce into one pending flag.
#[derive(Default)]
pub struct Doorbell {
    state: Mutex<(bool, bool)>, // (pending, stop)
    cv: std::sync::Condvar,
}

impl Doorbell {
    pub fn ring(&self) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !s.0 {
            s.0 = true;
            self.cv.notify_one();
        }
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.1 = true;
        self.cv.notify_one();
    }

    /// Blocks until rung (true) or stopped (false).
    pub fn wait(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if s.1 {
                return false;
            }
            if s.0 {
                s.0 = false;
                return true;
            }
            s = self.cv.wait(s).unwrap_or_else(|p| p.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::{CountLine, TestLine};

    #[test]
    fn interrupt_is_level() {
        let line = Arc::new(TestLine::default());
        let irq = Interrupt::new(line.clone());
        irq.raise(INT_USED_RING);
        assert!(line.level());
        irq.raise(INT_CONFIG);
        irq.ack(INT_USED_RING);
        assert!(line.level(), "config bit still set");
        irq.ack(INT_CONFIG);
        assert!(!line.level());
        assert_eq!(irq.status(), 0);
    }

    #[test]
    fn interrupt_drives_the_line_only_on_edges() {
        let line = Arc::new(CountLine::default());
        let irq = Interrupt::new(line.clone());
        irq.raise(0);
        irq.ack(INT_CONFIG);
        assert_eq!(line.calls(), 0, "nothing set, nothing cleared");
        for _ in 0..3 {
            irq.raise(INT_USED_RING);
        }
        irq.raise(INT_CONFIG);
        irq.ack(INT_CONFIG);
        irq.ack(INT_USED_RING);
        irq.ack(INT_USED_RING);
        assert_eq!((line.calls(), line.transitions(), line.level()), (2, 2, false));
    }

    /// Device threads raise while a driver thread acknowledges only what it read, and only
    /// while the line is up (a guest sees nothing else): a raise racing an ack must never leave
    /// a status bit set with the line down (the raiser would wait forever) or the line up with
    /// nothing set (an interrupt storm).
    #[test]
    fn a_raise_racing_an_ack_never_loses_the_level() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        /// Upper bound on rounds, and the wall-time budget after which a device stops; each
        /// device still runs `MIN_ROUNDS` first, so a loaded host exercises the race rather
        /// than passing on a handful of rounds. Quiet, 20k rounds take ~0.15 s; under load the
        /// same count took minutes, past the CI kill (#315).
        const ROUNDS: u32 = 20_000;
        const MIN_ROUNDS: u32 = 500;
        const BUDGET: Duration = Duration::from_secs(2);
        /// A bit that stays set this long is truly lost (the driver acks within microseconds
        /// when scheduled); generous so a starved driver is not reported as a lost level.
        const LOST_AFTER: Duration = Duration::from_secs(60);
        /// A controller call that takes a while (the real one is a hypervisor call plus a
        /// wake), so racing edges overlap in it.
        #[derive(Default)]
        struct Slow(CountLine);
        impl IrqLine for Slow {
            fn set(&self, level: bool) {
                std::thread::yield_now();
                self.0.set(level);
            }
        }
        impl std::ops::Deref for Slow {
            type Target = CountLine;
            fn deref(&self) -> &CountLine {
                &self.0
            }
        }
        let line = Arc::new(Slow::default());
        let irq = Interrupt::new(line.clone());
        let done = AtomicBool::new(false);
        let start = Instant::now();
        std::thread::scope(|s| {
            let (irq, line, done) = (&irq, &line, &done);
            let driver = s.spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if line.level() {
                        let st = irq.status();
                        irq.ack(st);
                    } else {
                        std::hint::spin_loop();
                    }
                }
            });
            let devices: Vec<_> = [INT_USED_RING, INT_CONFIG]
                .into_iter()
                .map(|bit| {
                    s.spawn(move || {
                        let mut round = 0;
                        while round < ROUNDS && (round < MIN_ROUNDS || start.elapsed() < BUDGET) {
                            irq.raise(bit);
                            let t = Instant::now();
                            while irq.status() & bit != 0 {
                                if t.elapsed() > LOST_AFTER {
                                    return Err(format!("bit {bit} lost its level in round {round}"));
                                }
                                std::thread::yield_now();
                            }
                            round += 1;
                        }
                        Ok(round)
                    })
                })
                .collect();
            let results: Vec<_> = devices.into_iter().map(|d| d.join().expect("device")).collect();
            // Before asserting, so a failure can't leave the driver spinning.
            done.store(true, Ordering::SeqCst);
            driver.join().expect("driver");
            for r in results {
                let rounds = r.expect("no lost level");
                assert!(rounds >= MIN_ROUNDS, "only {rounds} rounds completed");
            }
        });
        assert_eq!((irq.status(), line.level()), (0, false), "quiescent: line follows the status");
        assert_eq!(line.calls(), line.transitions(), "only edges reach the controller");
    }

    #[test]
    fn an_edge_interrupt_signals_every_used_ring_raise_and_holds_nothing() {
        let line = Arc::new(CountLine::default());
        let irq = Interrupt::edge(line.clone());
        for _ in 0..3 {
            irq.raise(INT_USED_RING);
        }
        irq.raise(INT_CONFIG);
        irq.ack(INT_USED_RING);
        assert_eq!((line.calls(), irq.status()), (3, 0), "one signal per used-ring raise, no status kept");
    }

    #[test]
    fn doorbell_coalesces_and_stops() {
        let d = Arc::new(Doorbell::default());
        d.ring();
        d.ring();
        assert!(d.wait());
        let d2 = d.clone();
        let t = std::thread::spawn(move || d2.wait());
        d.stop();
        assert!(!t.join().expect("join"));
    }
}
