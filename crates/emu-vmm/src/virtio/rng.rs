//! virtio-rng: entropy from the host's random source (`sim_core::entropy`: the kernel's on unix,
//! `ProcessPrng` on Windows, where there is no `/dev/urandom`).
use std::sync::Arc;

use super::queue::Queue;
use super::{Activation, ID_RNG, INT_USED_RING, Interrupt, VirtioDevice};
use emu_hvf::GuestRam;

pub struct Rng {
    active: Option<(Arc<GuestRam>, Arc<Interrupt>, Queue)>,
}

impl Rng {
    pub fn new() -> Rng {
        Rng { active: None }
    }
}

impl Default for Rng {
    fn default() -> Self {
        Rng::new()
    }
}

impl VirtioDevice for Rng {
    fn device_id(&self) -> u32 {
        ID_RNG
    }
    fn features(&self) -> u64 {
        0
    }
    fn num_queues(&self) -> usize {
        1
    }
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let cfg = act.queues.first().copied().flatten().ok_or("rng queue not enabled")?;
        self.active = Some((act.mem, act.irq, Queue::new(cfg, false)));
        Ok(())
    }
    // Requests are small and rare: serve them on the vCPU thread.
    fn notify(&mut self, _queue: u16) {
        let Some((mem, irq, q)) = self.active.as_mut() else { return };
        let mut used = false;
        while let Ok(Some(chain)) = q.pop(mem) {
            let want: usize = chain.writable().map(|d| d.len as usize).sum::<usize>().min(4096);
            let mut buf = vec![0u8; want];
            // A failure leaves zeros, as an unreadable /dev/urandom did.
            let _ = sim_core::entropy::fill(&mut buf);
            let n = chain.write_all(mem, &buf).unwrap_or(0);
            let _ = q.add_used(mem, chain.head, n);
            used = true;
        }
        if used && q.should_notify(mem) {
            irq.raise(INT_USED_RING);
        }
    }
    fn reset(&mut self) {
        self.active = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::TestLine;
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    #[test]
    fn fills_writable_buffers_up_to_4_kib_and_interrupts() {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let mut drv = Driver::new(8);
        let mut rng = Rng::new();
        assert_eq!((rng.device_id(), rng.num_queues(), rng.features()), (ID_RNG, 1, 0));
        rng.notify(0); // before activation: nothing to do
        rng.activate(Activation {
            mem: mem.clone(),
            irq: Arc::new(Interrupt::new(line.clone())),
            queues: vec![Some(drv.cfg)],
            features: 0,
        })
        .expect("activate");
        let (small, big) = (RAM_BASE + 0x8000, RAM_BASE + 0x9000);
        drv.push(&mem, &[(small, 32, true)]);
        drv.push(&mem, &[(big, 8192, true)]);
        rng.notify(0);
        assert_eq!(drv.used_idx(&mem), 2);
        assert_eq!(drv.used(&mem, 0), (0, 32));
        assert_eq!(drv.used(&mem, 1).1, 4096, "capped per request");
        let mut got = vec![0u8; 32];
        mem.read(small, &mut got).expect("read");
        assert!(got.iter().any(|&b| b != 0), "entropy, not zeros");
        assert!(line.level());
        rng.reset();
        drv.push(&mem, &[(small, 16, true)]);
        rng.notify(0);
        assert_eq!(drv.used_idx(&mem), 2, "a reset device serves nothing");
    }
}
