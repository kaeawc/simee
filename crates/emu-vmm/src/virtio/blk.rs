//! virtio-blk, served by one worker thread per disk so the vCPU that kicked the queue goes
//! straight back to the guest.
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use emu_hvf::GuestRam;

use super::mmio::{F_RING_EVENT_IDX, F_RING_INDIRECT_DESC};
use super::queue::{Chain, Desc, Queue};
use super::{Activation, Doorbell, ID_BLOCK, INT_USED_RING, Interrupt, VirtioDevice};
use crate::disk::Disk;

const F_SEG_MAX: u64 = 1 << 2;
const F_FLUSH: u64 = 1 << 9;
const F_DISCARD: u64 = 1 << 13;
const F_WRITE_ZEROES: u64 = 1 << 14;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const SECTOR: u64 = 512;
const QUEUE_SIZE: u16 = 256;
const SEG_MAX: u32 = 126;

/// Largest single discard or write-zeroes segment (~2 GiB), as QEMU offers.
const MAX_DWZ_SECTORS: u32 = 0x3f_ffff;
/// Linux merges adjacent discards into one multi-segment request; write-zeroes is always one.
const MAX_DISCARD_SEG: u32 = 32;
const MAX_WRITE_ZEROES_SEG: u32 = 1;
/// Discard granularity: one host filesystem block (4 KiB on APFS), the unit holes are punched in.
const DISCARD_ALIGNMENT_SECTORS: u32 = 8;
/// `virtio_blk_discard_write_zeroes`: le64 sector, le32 num_sectors, le32 flags.
const DWZ_SEGMENT: usize = 16;
const DWZ_F_UNMAP: u32 = 1;

pub struct Blk {
    disk: Arc<Mutex<Disk>>,
    capacity_sectors: u64,
    serial: String,
    worker: Option<(Arc<Doorbell>, JoinHandle<()>)>,
}

impl Blk {
    pub fn new(disk: Disk, serial: impl Into<String>) -> Blk {
        let capacity_sectors = disk.size() / SECTOR;
        Blk { disk: Arc::new(Mutex::new(disk)), capacity_sectors, serial: serial.into(), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some((bell, handle)) = self.worker.take() {
            bell.stop();
            let _ = handle.join();
        }
    }
}

impl Drop for Blk {
    /// The machine is stopping (a signal, the console's `kill`, a power-off or a reset): the
    /// worker finishes the request it is serving, then the disk syncs once, so what the guest
    /// wrote is on the drive when emu exits (#388).
    fn drop(&mut self) {
        self.stop_worker();
        let mut disk = self.disk.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = disk.sync() {
            eprintln!("emu: syncing disk {} at stop: {e}", self.serial);
        }
    }
}

impl VirtioDevice for Blk {
    fn device_id(&self) -> u32 {
        ID_BLOCK
    }

    fn features(&self) -> u64 {
        F_SEG_MAX | F_FLUSH | F_DISCARD | F_WRITE_ZEROES | F_RING_INDIRECT_DESC | F_RING_EVENT_IDX
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn queue_max_size(&self) -> u16 {
        QUEUE_SIZE
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 0x40];
        cfg[0x00..0x08].copy_from_slice(&self.capacity_sectors.to_le_bytes());
        cfg[0x0c..0x10].copy_from_slice(&SEG_MAX.to_le_bytes());
        cfg[0x24..0x28].copy_from_slice(&MAX_DWZ_SECTORS.to_le_bytes()); // max_discard_sectors
        cfg[0x28..0x2c].copy_from_slice(&MAX_DISCARD_SEG.to_le_bytes());
        cfg[0x2c..0x30].copy_from_slice(&DISCARD_ALIGNMENT_SECTORS.to_le_bytes());
        cfg[0x30..0x34].copy_from_slice(&MAX_DWZ_SECTORS.to_le_bytes()); // max_write_zeroes_sectors
        cfg[0x34..0x38].copy_from_slice(&MAX_WRITE_ZEROES_SEG.to_le_bytes());
        cfg[0x38] = 1; // write_zeroes_may_unmap: zeroed ranges become holes either way
        // Logical blocks stay 512 bytes (no VIRTIO_BLK_F_BLK_SIZE): the images' GPTs are laid
        // out in 512-byte LBAs.
        let off = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(off + i).copied().unwrap_or(0);
        }
    }

    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let cfg = act.queues.first().copied().flatten().ok_or("block request queue not enabled")?;
        self.stop_worker();
        let bell = Arc::new(Doorbell::default());
        let queue = Queue::new(cfg, act.features & F_RING_EVENT_IDX != 0);
        let mut w = Worker::new(act.mem, act.irq, queue, self.disk.clone(), self.capacity_sectors, self.serial.clone());
        let b = bell.clone();
        let handle = std::thread::Builder::new()
            .name(format!("blk-{}", self.serial))
            .spawn(move || {
                crate::device_thread();
                while b.wait() {
                    w.drain();
                }
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
    queue: Queue,
    disk: Arc<Mutex<Disk>>,
    capacity_sectors: u64,
    serial: String,
    /// Reused for every request: the popped chain and its data buffers as `(gpa, len)`.
    chain: Chain,
    segs: Vec<(u64, usize)>,
}

impl Worker {
    fn new(mem: Arc<GuestRam>, irq: Arc<Interrupt>, queue: Queue, disk: Arc<Mutex<Disk>>, capacity_sectors: u64, serial: String) -> Worker {
        Worker { mem, irq, queue, disk, capacity_sectors, serial, chain: Chain::default(), segs: Vec::new() }
    }

    /// Serves every published request. Kicks are off while it runs (the guest publishing more
    /// meanwhile costs it no MMIO exit); turning them back on rechecks the ring, so a request
    /// published just before that is served here rather than waiting for a kick that never comes.
    fn drain(&mut self) {
        let mut completed = false;
        let mut chain = std::mem::take(&mut self.chain);
        'session: loop {
            self.queue.disable_notify(&self.mem);
            loop {
                match self.queue.pop_into(&self.mem, &mut chain) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        eprintln!("emu: blk-{}: {e}", self.serial);
                        // Kicks back on, or the guest has no way left to wake the worker.
                        self.queue.enable_notify(&self.mem);
                        break 'session;
                    }
                }
                let written = self.handle(&chain);
                if let Err(e) = self.queue.add_used(&self.mem, chain.head, written) {
                    eprintln!("emu: blk-{}: {e}", self.serial);
                    self.queue.enable_notify(&self.mem);
                    break 'session;
                }
                completed = true;
            }
            if !self.queue.enable_notify(&self.mem) {
                break;
            }
        }
        self.chain = chain;
        if completed && self.queue.should_notify(&self.mem) {
            self.irq.raise(INT_USED_RING);
        }
    }

    /// Executes one request; returns the number of bytes written into guest buffers.
    fn handle(&mut self, chain: &Chain) -> u32 {
        let descs = &chain.descs;
        let Some(status) = descs.last().filter(|d| d.write && d.len >= 1) else {
            return 0;
        };
        let header = match descs.first().filter(|d| !d.write && d.len >= 16) {
            Some(h) => h,
            None => return 0,
        };
        let mut hdr = [0u8; 16];
        if self.mem.read(header.addr, &mut hdr).is_err() {
            return 0;
        }
        let typ = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap_or_default());
        let data = &descs[1..descs.len() - 1];
        let mut written = 0u32;
        let result: Result<(), u8> = (|| match typ {
            // The data buffers go to the disk as one vectored request (one preadv/pwritev).
            T_IN | T_OUT => {
                let off = sector.checked_mul(SECTOR).ok_or(S_IOERR)?;
                let want_write = typ == T_IN;
                self.segs.clear();
                for d in data {
                    if d.write != want_write {
                        return Err(S_IOERR);
                    }
                    self.segs.push((d.addr, d.len as usize));
                }
                if typ == T_IN {
                    let disk = self.disk.lock().map_err(|_| S_IOERR)?;
                    disk.read_vectored_to_guest(&self.mem, off, &self.segs).map_err(|_| S_IOERR)?;
                    written = data.iter().fold(0u32, |n, d| n.saturating_add(d.len));
                } else {
                    let mut disk = self.disk.lock().map_err(|_| S_IOERR)?;
                    disk.write_vectored_from_guest(&self.mem, off, &self.segs).map_err(|_| S_IOERR)?;
                }
                Ok(())
            }
            T_FLUSH => self.disk.lock().map_err(|_| S_IOERR)?.flush().map_err(|_| S_IOERR),
            T_GET_ID => {
                let mut id = [0u8; 20];
                let s = self.serial.as_bytes();
                id[..s.len().min(20)].copy_from_slice(&s[..s.len().min(20)]);
                let d = data.first().filter(|d| d.write).ok_or(S_IOERR)?;
                let n = (d.len as usize).min(20);
                self.mem.write(d.addr, &id[..n]).map_err(|_| S_IOERR)?;
                written += n as u32;
                Ok(())
            }
            T_DISCARD | T_WRITE_ZEROES => self.discard_write_zeroes(typ, data),
            _ => Err(S_UNSUPP),
        })();
        let code = match result {
            Ok(()) => S_OK,
            Err(c) => c,
        };
        if self.mem.write(status.addr, &[code]).is_ok() {
            written += 1;
        }
        written
    }

    /// DISCARD and WRITE_ZEROES: both make the ranges read back as zeroes and free their host
    /// storage (a discarded range may read back as anything, so zeroes are fine). The whole
    /// request is validated before any segment is applied.
    fn discard_write_zeroes(&self, typ: u32, data: &[Desc]) -> Result<(), u8> {
        let (max_seg, allowed_flags) = if typ == T_DISCARD { (MAX_DISCARD_SEG, 0) } else { (MAX_WRITE_ZEROES_SEG, DWZ_F_UNMAP) };
        if data.iter().any(|d| d.write) {
            return Err(S_IOERR);
        }
        let total: usize = data.iter().map(|d| d.len as usize).sum();
        if total == 0 || !total.is_multiple_of(DWZ_SEGMENT) {
            return Err(S_IOERR);
        }
        if total / DWZ_SEGMENT > max_seg as usize {
            return Err(S_UNSUPP);
        }
        let mut payload = vec![0u8; total];
        let mut at = 0;
        for d in data {
            self.mem.read(d.addr, &mut payload[at..at + d.len as usize]).map_err(|_| S_IOERR)?;
            at += d.len as usize;
        }
        let mut ranges = Vec::with_capacity(total / DWZ_SEGMENT);
        for seg in payload.as_chunks::<DWZ_SEGMENT>().0 {
            let sector = u64::from_le_bytes(seg[0..8].try_into().map_err(|_| S_IOERR)?);
            let num = u32::from_le_bytes(seg[8..12].try_into().map_err(|_| S_IOERR)?);
            let flags = u32::from_le_bytes(seg[12..16].try_into().map_err(|_| S_IOERR)?);
            if flags & !allowed_flags != 0 {
                return Err(S_UNSUPP);
            }
            match sector.checked_add(u64::from(num)) {
                Some(end) if num <= MAX_DWZ_SECTORS && end <= self.capacity_sectors => {}
                _ => return Err(S_IOERR),
            }
            ranges.push((sector * SECTOR, u64::from(num) * SECTOR));
        }
        let mut disk = self.disk.lock().map_err(|_| S_IOERR)?;
        for (offset, len) in ranges {
            disk.zero_range(offset, len).map_err(|_| S_IOERR)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::TestLine;
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    fn request(mem: &GuestRam, drv: &mut Driver, typ: u32, sector: u64, data: (u64, u32, bool)) -> u16 {
        let hdr = RAM_BASE + 0x20000;
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&typ.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        mem.write(hdr, &h).expect("hdr");
        drv.push(mem, &[(hdr, 16, false), data, (RAM_BASE + 0x21000, 1, true)])
    }

    fn worker(blk: &Blk, mem: &Arc<GuestRam>, irq: &Arc<Interrupt>, drv: &Driver) -> Worker {
        Worker::new(mem.clone(), irq.clone(), Queue::new(drv.cfg, false), blk.disk.clone(), blk.capacity_sectors, blk.serial.clone())
    }

    fn status(mem: &GuestRam) -> u8 {
        let mut st = [0u8; 1];
        mem.read(RAM_BASE + 0x21000, &mut st).expect("status");
        st[0]
    }

    /// Writes `virtio_blk_discard_write_zeroes` segments at `addr`; returns the payload length.
    fn segments(mem: &GuestRam, addr: u64, segs: &[(u64, u32, u32)]) -> u32 {
        for (i, &(sector, num, flags)) in segs.iter().enumerate() {
            let mut s = [0u8; 16];
            s[..8].copy_from_slice(&sector.to_le_bytes());
            s[8..12].copy_from_slice(&num.to_le_bytes());
            s[12..].copy_from_slice(&flags.to_le_bytes());
            mem.write(addr + 16 * i as u64, &s).expect("segment");
        }
        16 * segs.len() as u32
    }

    /// Device-side cost of reads, as the guest issues them during boot: batches of requests at
    /// spread sectors, each popped, served by `pread` into guest RAM, and completed.
    ///     cargo test --release -p simee-emu-vmm blk_read_throughput -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn blk_read_throughput() {
        use std::time::Instant;
        const FILE: u64 = 64 << 20;
        let path = std::env::temp_dir().join(format!("emu-blk-bench-{}.img", std::process::id()));
        let data: Vec<u8> = (0..FILE).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        std::fs::write(&path, &data).expect("image");
        for (kind, disk) in [("persistent", Disk::persistent(&path)), ("ephemeral", Disk::ephemeral(&path))] {
            let disk = disk.expect("disk");
            let blk = Blk::new(disk, "bench");
            let mem = Arc::new(GuestRam::new(RAM_BASE, 64 << 20).expect("ram"));
            let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
            for (size, total) in [(4096u32, 64u64 << 20), (65536, 512 << 20)] {
                let mut drv = Driver::new(256);
                let mut w = worker(&blk, &mem, &irq, &drv);
                let batch = 64u64;
                let start = Instant::now();
                let mut done = 0u64;
                let mut n = 0u64;
                while done < total {
                    for i in 0..batch {
                        let hdr = RAM_BASE + 0x10_0000 + i * 32;
                        let sector = (n * 7919 % (FILE / u64::from(size))) * u64::from(size) / 512;
                        let mut h = [0u8; 16];
                        h[..4].copy_from_slice(&T_IN.to_le_bytes());
                        h[8..].copy_from_slice(&sector.to_le_bytes());
                        mem.write(hdr, &h).expect("hdr");
                        let buf = RAM_BASE + 0x20_0000 + i * u64::from(size);
                        drv.push(&mem, &[(hdr, 16, false), (buf, size, true), (hdr + 16, 1, true)]);
                        n += 1;
                    }
                    w.drain();
                    done += batch * u64::from(size);
                }
                let secs = start.elapsed().as_secs_f64();
                println!(
                    "blk {kind:<10} {:>3} KiB reads: {:>7.0} req/s, {:>6.0} MiB/s, {:>5.1} us/req",
                    size / 1024,
                    n as f64 / secs,
                    (done >> 20) as f64 / secs,
                    secs * 1e6 / n as f64
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Linux-shaped requests: one indirect table of a header, 32 scattered 4 KiB pages (every
    /// other page, so no two are guest-adjacent) and a status byte, i.e. one 128 KiB request.
    /// Reads and writes, persistent and ephemeral.
    ///     cargo test --release -p simee-emu-vmm blk_scattered_throughput -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn blk_scattered_throughput() {
        use std::time::Instant;
        const FILE: u64 = 64 << 20;
        const SEGS: u64 = 32;
        const REQ: u64 = SEGS * 4096;
        let path = std::env::temp_dir().join(format!("emu-blk-sg-bench-{}.img", std::process::id()));
        let data: Vec<u8> = (0..FILE).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        std::fs::write(&path, &data).expect("image");
        for (kind, disk) in [("persistent", Disk::persistent(&path)), ("ephemeral", Disk::ephemeral(&path))] {
            let blk = Blk::new(disk.expect("disk"), "bench");
            let mem = Arc::new(GuestRam::new(RAM_BASE, 64 << 20).expect("ram"));
            let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
            // Reads of a clean disk, writes, then reads again (an ephemeral overlay now covers it).
            for (op, typ) in [("read", T_IN), ("write", T_OUT), ("reread", T_IN)] {
                let mut drv = Driver::new(256);
                let mut w = worker(&blk, &mem, &irq, &drv);
                let (batch, total) = (32u64, 512u64 << 20);
                let start = Instant::now();
                let (mut done, mut n) = (0u64, 0u64);
                while done < total {
                    for i in 0..batch {
                        let hdr = RAM_BASE + 0x10_0000 + i * 32;
                        let sector = (n * 7919 % (FILE / REQ)) * REQ / 512;
                        let mut h = [0u8; 16];
                        h[..4].copy_from_slice(&typ.to_le_bytes());
                        h[8..].copy_from_slice(&sector.to_le_bytes());
                        mem.write(hdr, &h).expect("hdr");
                        let bufs: Vec<(u64, u32, bool)> = std::iter::once((hdr, 16, false))
                            .chain((0..SEGS).map(|s| (RAM_BASE + 0x40_0000 + (i * SEGS + s) * 0x2000, 4096, typ == T_IN)))
                            .chain(std::iter::once((hdr + 16, 1, true)))
                            .collect();
                        drv.push_indirect(&mem, RAM_BASE + 0x20_0000 + i * 0x400, &bufs);
                        n += 1;
                    }
                    w.drain();
                    done += batch * REQ;
                }
                let secs = start.elapsed().as_secs_f64();
                let mut st = [0u8; 1];
                mem.read(RAM_BASE + 0x10_0010, &mut st).expect("status");
                assert_eq!(st[0], S_OK, "{kind} {op}");
                println!(
                    "blk {kind:<10} {op:<6} 32x4KiB: {:>6.0} req/s, {:>6.0} MiB/s, {:>6.1} us/req",
                    n as f64 / secs,
                    (done >> 20) as f64 / secs,
                    secs * 1e6 / n as f64
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Runs `reqs` reads of `segs` scattered 4 KiB pages each, with a driver thread publishing
    /// (up to `depth` in flight) while the worker thread drains, the way a guest and the blk
    /// worker race. The driver kicks whenever Linux's `virtqueue_kick_prepare` would. Returns
    /// (seconds, kicks). Panics if the driver waits 10 s without a completion: a lost wakeup.
    fn race(event_idx: bool, reqs: u32, depth: u16, segs: u64) -> (f64, u32) {
        use std::time::{Duration, Instant};
        let path = std::env::temp_dir().join(format!("emu-blk-race-{}-{event_idx}-{reqs}-{depth}-{segs}.img", std::process::id()));
        std::fs::write(&path, vec![5u8; 1 << 20]).expect("image");
        let blk = Blk::new(Disk::persistent(&path).expect("disk"), "race");
        let mem = Arc::new(GuestRam::new(RAM_BASE, 64 << 20).expect("ram"));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let mut drv = Driver::new(256);
        let mut w = worker(&blk, &mem, &irq, &drv);
        w.queue = Queue::new(drv.cfg, event_idx);
        let bell = Arc::new(Doorbell::default());
        let b = bell.clone();
        let handle = std::thread::spawn(move || {
            while b.wait() {
                w.drain();
            }
        });
        let (mut kicks, mut sent) = (0u32, 0u32);
        let (mut last_used, mut last_progress) = (0u16, Instant::now());
        let start = Instant::now();
        loop {
            let used = drv.used_idx(&mem);
            if used != last_used {
                (last_used, last_progress) = (used, Instant::now());
            }
            if u32::from(used) == reqs & 0xffff && sent == reqs {
                break;
            }
            assert!(last_progress.elapsed() < Duration::from_secs(10), "lost wakeup: {sent} sent, used.idx {used}, {kicks} kicks");
            if sent < reqs && drv.avail_idx().wrapping_sub(used) < depth {
                let slot = u64::from(sent % u32::from(depth));
                let hdr = RAM_BASE + 0x10_0000 + slot * 32;
                let mut h = [0u8; 16];
                h[8..].copy_from_slice(&(u64::from(sent) * segs * 8 % (2048 - segs * 8)).to_le_bytes());
                mem.write(hdr, &h).expect("hdr");
                let bufs: Vec<(u64, u32, bool)> = std::iter::once((hdr, 16, false))
                    .chain((0..segs).map(|s| (RAM_BASE + 0x40_0000 + (slot * segs + s) * 0x2000, 4096, true)))
                    .chain(std::iter::once((hdr + 16, 1, true)))
                    .collect();
                let old = drv.avail_idx();
                drv.push_indirect(&mem, RAM_BASE + 0x20_0000 + slot * 0x400, &bufs);
                sent += 1;
                if drv.needs_kick(&mem, old, event_idx) {
                    kicks += 1;
                    bell.ring();
                }
            } else {
                std::hint::spin_loop();
            }
        }
        let secs = start.elapsed().as_secs_f64();
        bell.stop();
        handle.join().expect("worker");
        let _ = std::fs::remove_file(&path);
        (secs, kicks)
    }

    /// The driver and the worker race on avail.idx and the notification state: every request
    /// completes (no lost wakeup), with and without event-idx.
    #[test]
    fn racing_driver_never_loses_a_wakeup() {
        for event_idx in [true, false] {
            for depth in [1, 4, 64] {
                race(event_idx, 3000, depth, 1);
                race(event_idx, 300, depth, 8);
            }
        }
    }

    /// Kicks (MMIO exits on a real guest) per request while the worker drains.
    ///     cargo test --release -p simee-emu-vmm blk_kicks_while_draining -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn blk_kicks_while_draining() {
        const N: u32 = 50_000;
        for (event_idx, segs) in [(true, 1u64), (true, 32), (false, 32)] {
            for depth in [1u16, 2, 4, 16, 64] {
                let (secs, kicks) = race(event_idx, N, depth, segs);
                println!(
                    "blk race event_idx={event_idx:<5} {segs:>2}x4KiB depth {depth:>2}: {:>7.0} req/s, {:.3} kicks/req",
                    f64::from(N) / secs,
                    f64::from(kicks) / f64::from(N)
                );
            }
        }
    }

    #[test]
    fn discard_and_write_zeroes() {
        const SECTORS: u64 = 128;
        let path = std::env::temp_dir().join(format!("emu-blk-dwz-{}", std::process::id()));
        std::fs::write(&path, vec![7u8; (SECTORS * SECTOR) as usize]).expect("write");
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let mut drv = Driver::new(64);
        let blk = Blk::new(Disk::persistent(&path).expect("open"), "vdc");
        assert_eq!(blk.features() & (F_DISCARD | F_WRITE_ZEROES), F_DISCARD | F_WRITE_ZEROES);
        let mut cfg = [0u8; 0x18];
        blk.read_config(0x24, &mut cfg);
        assert_eq!(&cfg[0..4], &MAX_DWZ_SECTORS.to_le_bytes(), "max_discard_sectors");
        assert_eq!(&cfg[8..12], &8u32.to_le_bytes(), "discard_sector_alignment");
        assert_eq!(cfg[0x14], 1, "write_zeroes_may_unmap");
        let mut w = worker(&blk, &mem, &irq, &drv);
        let seg = RAM_BASE + 0x30000;
        let mut send = |w: &mut Worker, typ: u32, segs: &[(u64, u32, u32)]| {
            let len = segments(&mem, seg, segs);
            request(&mem, &mut drv, typ, 0, (seg, len, false));
            w.drain();
            status(&mem)
        };

        // Unaligned write-zeroes (punch path writes the zeroes), an aligned discard (a real
        // hole), a two-segment discard and a write-zeroes that allows unmapping.
        assert_eq!(send(&mut w, T_WRITE_ZEROES, &[(1, 2, 0)]), S_OK);
        assert_eq!(send(&mut w, T_DISCARD, &[(16, 64, 0)]), S_OK);
        assert_eq!(send(&mut w, T_DISCARD, &[(100, 4, 0), (110, 2, 0)]), S_OK);
        assert_eq!(send(&mut w, T_WRITE_ZEROES, &[(120, 8, DWZ_F_UNMAP)]), S_OK);
        let data = std::fs::read(&path).expect("read");
        let zeroed =
            |s: u64| (1..3).contains(&s) || (16..80).contains(&s) || (100..104).contains(&s) || (110..112).contains(&s) || s >= 120;
        for s in 0..SECTORS {
            let want = if zeroed(s) { 0 } else { 7 };
            let bytes = &data[(s * SECTOR) as usize..((s + 1) * SECTOR) as usize];
            assert!(bytes.iter().all(|&b| b == want), "sector {s}");
        }

        // Rejections leave the disk alone.
        assert_eq!(send(&mut w, T_DISCARD, &[(0, 1, DWZ_F_UNMAP)]), S_UNSUPP, "unmap flag on discard");
        assert_eq!(send(&mut w, T_WRITE_ZEROES, &[(0, 1, 2)]), S_UNSUPP, "unknown flag");
        assert_eq!(send(&mut w, T_WRITE_ZEROES, &[(0, 1, 0), (4, 1, 0)]), S_UNSUPP, "too many segments");
        assert_eq!(send(&mut w, T_DISCARD, &[(0, 1, 0), (SECTORS - 1, 2, 0)]), S_IOERR, "past the end");
        assert_eq!(send(&mut w, T_WRITE_ZEROES, &[(u64::MAX, 2, 0)]), S_IOERR, "overflow");
        request(&mem, &mut drv, T_DISCARD, 0, (seg, 8, false)); // truncated segment
        w.drain();
        assert_eq!(status(&mem), S_IOERR);
        assert_eq!(std::fs::read(&path).expect("read")[0], 7, "first sector untouched");
        let _ = std::fs::remove_file(path);
    }

    /// A Linux-shaped request (indirect table, scattered pages) is served as one vectored
    /// request: data lands in each page in order, and a buffer of the wrong direction fails it.
    #[test]
    fn scattered_requests_read_and_write_every_segment() {
        let path = std::env::temp_dir().join(format!("emu-blk-sg-{}", std::process::id()));
        let image: Vec<u8> = (0..64 * 1024u32).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&path, &image).expect("write");
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let mut drv = Driver::new(16);
        let blk = Blk::new(Disk::persistent(&path).expect("open"), "vdd");
        let mut w = worker(&blk, &mem, &irq, &drv);
        let (hdr, st) = (RAM_BASE + 0x20000, RAM_BASE + 0x21000);
        let pages = [(RAM_BASE + 0x40000, 4096u32), (RAM_BASE + 0x42000, 512), (RAM_BASE + 0x42200, 1024), (RAM_BASE + 0x48000, 2560)];
        let send = |w: &mut Worker, drv: &mut Driver, typ: u32, sector: u64, write: bool| {
            let mut h = [0u8; 16];
            h[..4].copy_from_slice(&typ.to_le_bytes());
            h[8..].copy_from_slice(&sector.to_le_bytes());
            mem.write(hdr, &h).expect("hdr");
            let bufs: Vec<(u64, u32, bool)> = std::iter::once((hdr, 16, false))
                .chain(pages.iter().map(|&(a, l)| (a, l, write)))
                .chain(std::iter::once((st, 1, true)))
                .collect();
            let i = drv.avail_idx();
            drv.push_indirect(&mem, RAM_BASE + 0x6000 + u64::from(i) * 0x100, &bufs);
            w.drain();
            (status(&mem), drv.used(&mem, i).1)
        };
        // Read 8 KiB from sector 3.
        assert_eq!(send(&mut w, &mut drv, T_IN, 3, true), (S_OK, 8192 + 1));
        let mut at = 3 * 512;
        for &(a, l) in &pages {
            let mut got = vec![0u8; l as usize];
            mem.read(a, &mut got).expect("r");
            assert_eq!(got, image[at..at + l as usize], "page {a:#x}");
            at += l as usize;
        }
        // Write them back at sector 40, then check the file.
        assert_eq!(send(&mut w, &mut drv, T_OUT, 40, false), (S_OK, 1));
        let file = std::fs::read(&path).expect("read");
        assert_eq!(file[40 * 512..40 * 512 + 8192], image[3 * 512..3 * 512 + 8192]);
        // A device-writable buffer in a write request fails it before anything is written.
        assert_eq!(send(&mut w, &mut drv, T_OUT, 0, true).0, S_IOERR);
        assert_eq!(std::fs::read(&path).expect("read")[..512], image[..512]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_stopping_device_syncs_its_disk_once_its_worker_has_stopped() {
        let path = std::env::temp_dir().join(format!("emu-blk-stop-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 8192]).expect("write");
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let mut drv = Driver::new(16);
        let mut blk = Blk::new(Disk::persistent(&path).expect("open"), "vdb");
        blk.activate(Activation { mem: mem.clone(), irq, queues: vec![Some(drv.cfg)], features: 0 }).expect("activate");
        let buf = RAM_BASE + 0x30000;
        mem.write(buf, &[5u8; 512]).expect("w");
        mem.write(RAM_BASE + 0x21000, &[0xff]).expect("status");
        request(&mem, &mut drv, T_OUT, 3, (buf, 512, false));
        blk.notify(0);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while status(&mem) != S_OK {
            assert!(std::time::Instant::now() < until, "the worker never served the request");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let disk = blk.disk.clone();
        drop(blk);
        assert_eq!(disk.lock().expect("disk").syncs(), 1, "the stop synced the disk once");
        assert_eq!(std::fs::read(&path).expect("read")[3 * 512..4 * 512], [5u8; 512]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_writes_and_reports_status() {
        let path = std::env::temp_dir().join(format!("emu-blk-{}", std::process::id()));
        std::fs::write(&path, vec![7u8; 8192]).expect("write");
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let irq = Arc::new(Interrupt::new(line.clone()));
        let mut drv = Driver::new(16);
        let blk = Blk::new(Disk::persistent(&path).expect("open"), "vda");
        let mut w = worker(&blk, &mem, &irq, &drv);
        let buf = RAM_BASE + 0x30000;
        mem.write(buf, &[9u8; 512]).expect("w");
        request(&mem, &mut drv, T_OUT, 2, (buf, 512, false));
        w.drain();
        assert_eq!(mem.read_u16(RAM_BASE + 0x21000).expect("status") & 0xff, u16::from(S_OK));
        assert!(line.level());
        request(&mem, &mut drv, T_IN, 1, (buf, 1024, true));
        w.drain();
        let mut got = vec![0u8; 1024];
        mem.read(buf, &mut got).expect("r");
        assert!(got[..512].iter().all(|&b| b == 7) && got[512..].iter().all(|&b| b == 9));
        assert_eq!(drv.used(&mem, 1).1, 1025);
        request(&mem, &mut drv, T_IN, 16, (buf, 512, true)); // past the end
        w.drain();
        assert_eq!(status(&mem), S_IOERR);
        request(&mem, &mut drv, 99, 0, (buf, 512, true));
        w.drain();
        assert_eq!(status(&mem), S_UNSUPP);
        let _ = std::fs::remove_file(path);
    }
}
