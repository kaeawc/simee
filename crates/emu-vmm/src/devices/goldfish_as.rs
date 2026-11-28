//! Goldfish address space device (PCI 607d:f153): carves a large guest-physical area into
//! blocks for host-shared memory. Gralloc's host memory allocator requires it.
//!
//! The area BAR is backed by one lazily committed host mapping, so "allocating host memory" for
//! a block needs no work: pages cost nothing until the guest touches them. Pings run per-handle
//! contexts; the first ping on a handle selects its type (`metadata`).
//!
//! Every block reads as zero when it's handed out, like stock's freshly allocated host memory,
//! without the vCPU that asked for it clearing megabytes under the device lock. Free space is
//! either *clean* (never handed out, or cleared since) or *dirty* (freed with the guest's data
//! still in it). Allocations take clean space; a background thread clears dirty space in chunks
//! with the lock released. Only when no clean extent fits does an allocation clear memory
//! itself, still with the lock released. Both kinds are indexed by size: a lookup is a best fit
//! in O(log n), not a scan. The thread gives runs of 1 MiB or more back to the host
//! ([`GuestRam::discard`]: fresh zero pages for both the host and the guest) and zeroes what's
//! left with plain stores, so freed gralloc buffers stop costing host memory. Allocations that
//! clear for themselves zero in place: that memory goes straight back into use.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use emu_hvf::{DISCARD_GRANULE, GuestRam};

use crate::bus::{MmioDevice, le_value, set_le};

pub const VENDOR: u16 = 0x607d;
pub const DEVICE: u16 = 0xf153;

const REG_COMMAND: u64 = 0;
const REG_STATUS: u64 = 4;
const REG_GUEST_PAGE_SIZE: u64 = 8;
const REG_BLOCK_SIZE_LOW: u64 = 12;
const REG_BLOCK_SIZE_HIGH: u64 = 16;
const REG_BLOCK_OFFSET_LOW: u64 = 20;
const REG_BLOCK_OFFSET_HIGH: u64 = 24;
const REG_PING: u64 = 28;
const REG_PING_INFO_ADDR_LOW: u64 = 32;
const REG_PING_INFO_ADDR_HIGH: u64 = 36;
const REG_HANDLE: u64 = 40;
const REG_PHYS_START_LOW: u64 = 44;
const REG_PHYS_START_HIGH: u64 = 48;
const REG_PING_WITH_DATA: u64 = 52;

const CMD_ALLOCATE_BLOCK: u32 = 1;
const CMD_DEALLOCATE_BLOCK: u32 = 2;
const CMD_GEN_HANDLE: u32 = 3;
const CMD_DESTROY_HANDLE: u32 = 4;
const CMD_TELL_PING_INFO_ADDR: u32 = 5;

/// Context types a handle can take (first ping's `metadata`).
mod ctx {
    pub const HOST_MEMORY_ALLOCATOR: u64 = 5;
    pub const SHARED_SLOTS_HOST_MEMORY_ALLOCATOR: u64 = 6;
}

/// Host memory allocator commands (ping `metadata` on an allocator handle).
mod hma {
    pub const ALLOCATE: u64 = 1;
    pub const UNALLOCATE: u64 = 2;
    pub const CHECK_SHARED_SLOTS: u64 = 3;
}

/// Bytes cleared per trip through the lock: an allocation that waits on one stalls for a
/// fraction of a millisecond, not for a whole freed buffer.
const CLEAR_CHUNK: u64 = 2 << 20;

/// Freed runs at least this long go back to the host instead of being zeroed in place.
const DISCARD_MIN: u64 = 1 << 20;

/// Makes `[off, off + len)` of the area read as zero. Long runs give their whole host pages back
/// ([`GuestRam::discard`]) and zero only the partial pages at the ends; anything shorter, or a
/// discard that fails, is zeroed in place. Only for free space: the guest must not be using it.
/// False when the run must never be handed out again: the discard left it unmapped from the
/// guest ([`emu_hvf::Error::GuestHole`]), so a buffer there would be unreachable.
#[cfg(test)]
fn clear(area: &GuestRam, off: u64, len: u64) -> bool {
    clear_with(area, off, len, GuestRam::discard)
}

/// How freed runs go back to the host: [`GuestRam::discard`], or a stand-in in tests.
type Discard = fn(&GuestRam, u64, u64) -> emu_hvf::Result<u64>;

fn clear_with(area: &GuestRam, off: u64, len: u64, discard: Discard) -> bool {
    let (start, end) = (area.base() + off, area.base() + off + len);
    if len >= DISCARD_MIN {
        let (lo, hi) = (start.next_multiple_of(DISCARD_GRANULE), end / DISCARD_GRANULE * DISCARD_GRANULE);
        if hi > lo {
            match discard(area, lo, hi - lo) {
                Ok(_) => {
                    let _ = area.zero(start, (lo - start) as usize);
                    let _ = area.zero(hi, (end - hi) as usize);
                    return true;
                }
                Err(e @ emu_hvf::Error::GuestHole { .. }) => {
                    eprintln!("emu: address space: leaking {len:#x} bytes at offset {off:#x}: {e}");
                    return false;
                }
                Err(_) => {}
            }
        }
    }
    let _ = area.zero(start, len as usize);
    true
}

/// Free extents of one kind, indexed by offset (to merge neighbours) and by size (best fit).
/// Adjacent extents are always merged, so no two touch.
#[derive(Default)]
struct Extents {
    by_off: BTreeMap<u64, u64>,
    by_size: BTreeSet<(u64, u64)>,
}

impl Extents {
    fn add(&mut self, off: u64, len: u64) {
        self.by_off.insert(off, len);
        self.by_size.insert((len, off));
    }

    fn remove(&mut self, off: u64) -> Option<u64> {
        let len = self.by_off.remove(&off)?;
        self.by_size.remove(&(len, off));
        Some(len)
    }

    /// Frees `[off, off + len)`, merging it with its neighbours.
    fn insert(&mut self, off: u64, len: u64) {
        let (mut start, mut size) = (off, len);
        if let Some((&prev, &plen)) = self.by_off.range(..off).next_back()
            && prev + plen == off
        {
            self.remove(prev);
            start = prev;
            size += plen;
        }
        if let Some(next) = self.remove(off + len) {
            size += next;
        }
        self.add(start, size);
    }

    /// Takes `size` bytes from the front of the smallest extent that holds them (the lowest
    /// among equals). Best fit reuses holes before splitting the large untouched tail.
    fn take(&mut self, size: u64) -> Option<u64> {
        let &(len, off) = self.by_size.range((size, 0)..).next()?;
        self.take_at(off, len, size);
        Some(off)
    }

    /// Takes up to `max` bytes from the front of the lowest extent.
    fn take_front(&mut self, max: u64) -> Option<(u64, u64)> {
        let (&off, &len) = self.by_off.first_key_value()?;
        let n = len.min(max);
        self.take_at(off, len, n);
        Some((off, n))
    }

    fn take_at(&mut self, off: u64, len: u64, n: u64) {
        self.remove(off);
        if len > n {
            // The rest keeps the extent's far end, which touched nothing of this kind.
            self.add(off + n, len - n);
        }
    }

    fn is_empty(&self) -> bool {
        self.by_off.is_empty()
    }
}

#[derive(Default)]
struct State {
    regs: HashMap<u64, u32>,
    page_size: u64,
    /// Blocks handed out: offset → size.
    used: BTreeMap<u64, u64>,
    /// Free and reading as zero: never handed out, or cleared since it was freed.
    clean: Extents,
    /// Freed with the guest's data still in it, waiting to be cleared.
    dirty: Extents,
    /// Bytes taken out of `dirty` that someone is clearing with the lock released.
    clearing: u64,
    next_handle: u32,
    /// handle → (ping info guest address, context type once set)
    handles: HashMap<u32, (u64, Option<u64>)>,
    stop: bool,
}

type Guard<'a> = MutexGuard<'a, State>;

/// What the device, its MMIO exits and its clearing thread share.
struct Shared {
    state: Mutex<State>,
    area: Arc<GuestRam>,
    /// Wakes the clearing thread: dirty space was freed, or it's time to stop.
    work: Condvar,
    /// Wakes allocations waiting for a chunk someone else is clearing.
    cleared: Condvar,
    discard: Discard,
}

impl Shared {
    fn lock(&self) -> Guard<'_> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Clears one chunk of dirty space with the lock released; returns with it held again.
    fn clear_chunk<'a>(&'a self, mut s: Guard<'a>) -> Guard<'a> {
        let Some((off, len)) = s.dirty.take_front(CLEAR_CHUNK) else { return s };
        s.clearing += len;
        drop(s);
        let reusable = clear_with(&self.area, off, len, self.discard);
        let mut s = self.lock();
        s.clearing -= len;
        // A run the guest can no longer reach stays out of the allocator for good.
        if reusable {
            s.clean.insert(off, len);
        }
        self.cleared.notify_all();
        s
    }

    /// The clearing thread: turns dirty space clean until the device goes away.
    fn clear_loop(&self) {
        let mut s = self.lock();
        while !s.stop {
            s = if s.dirty.is_empty() { self.work.wait(s).unwrap_or_else(PoisonError::into_inner) } else { self.clear_chunk(s) };
        }
    }
}

pub struct GoldfishAddressSpace {
    shared: Arc<Shared>,
    mem: Arc<GuestRam>,
    /// The clearing thread. Without it (it failed to start) allocations clear what they need.
    clearer: Option<JoinHandle<()>>,
}

impl GoldfishAddressSpace {
    pub fn new(mem: Arc<GuestRam>, area: Arc<GuestRam>) -> GoldfishAddressSpace {
        Self::build(mem, area, true)
    }

    fn build(mem: Arc<GuestRam>, area: Arc<GuestRam>, clearer: bool) -> GoldfishAddressSpace {
        Self::build_with(mem, area, clearer, GuestRam::discard)
    }

    fn build_with(mem: Arc<GuestRam>, area: Arc<GuestRam>, clearer: bool, discard: Discard) -> GoldfishAddressSpace {
        let mut st = State { page_size: 4096, next_handle: 1, ..Default::default() };
        // The whole area starts untouched, so it all reads as zero.
        st.clean.insert(0, area.len());
        let shared = Arc::new(Shared { state: Mutex::new(st), area, work: Condvar::new(), cleared: Condvar::new(), discard });
        let clearer = clearer
            .then(|| {
                let sh = shared.clone();
                std::thread::Builder::new()
                    .name("addrspace-clear".into())
                    .spawn(move || {
                        // Housekeeping: an allocation that finds nothing cleared zeroes for itself,
                        // so the guest never waits on this thread.
                        emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
                        sh.clear_loop()
                    })
                    .ok()
            })
            .flatten();
        GoldfishAddressSpace { shared, mem, clearer }
    }

    /// Waits until no freed space is left to clear.
    #[cfg(test)]
    fn settle(&self) {
        let mut s = self.shared.lock();
        while !s.dirty.is_empty() || s.clearing > 0 {
            s = if self.clearer.is_some() {
                self.shared.cleared.wait(s).unwrap_or_else(PoisonError::into_inner)
            } else {
                self.shared.clear_chunk(s)
            };
        }
    }

    fn area(&self) -> &GuestRam {
        &self.shared.area
    }

    fn reg(s: &State, r: u64) -> u32 {
        s.regs.get(&r).copied().unwrap_or(0)
    }

    fn reg64(s: &State, lo: u64, hi: u64) -> u64 {
        u64::from(Self::reg(s, lo)) | (u64::from(Self::reg(s, hi)) << 32)
    }

    fn set64(s: &mut State, lo: u64, hi: u64, v: u64) {
        s.regs.insert(lo, v as u32);
        s.regs.insert(hi, (v >> 32) as u32);
    }

    /// Hands out `size` bytes, rounded up to guest pages, that read as zero: `(offset, size)`.
    /// Takes clean space when any fits. Otherwise it clears a dirty extent itself, or helps clear
    /// until merged space fits, always with the lock released.
    fn allocate<'a>(&'a self, mut s: Guard<'a>, size: u64) -> (Guard<'a>, Option<(u64, u64)>) {
        let Some(size) = size.max(1).checked_next_multiple_of(s.page_size) else { return (s, None) };
        loop {
            if let Some(off) = s.clean.take(size) {
                s.used.insert(off, size);
                return (s, Some((off, size)));
            }
            if let Some(off) = s.dirty.take(size) {
                s.used.insert(off, size);
                drop(s);
                let _ = self.area().zero(self.area().base() + off, size as usize);
                return (self.shared.lock(), Some((off, size)));
            }
            s = if !s.dirty.is_empty() {
                self.shared.clear_chunk(s)
            } else if s.clearing > 0 {
                self.shared.cleared.wait(s).unwrap_or_else(PoisonError::into_inner)
            } else {
                return (s, None);
            };
        }
    }

    /// Frees a block; the clearing thread clears it before it's handed out again. The thread
    /// waits only while nothing is dirty, so only that transition wakes it, after unlocking (it
    /// would otherwise wake just to wait for the lock).
    fn deallocate(&self, mut s: Guard<'_>, off: u64) -> bool {
        let Some(size) = s.used.remove(&off) else { return false };
        let idle = s.dirty.is_empty();
        s.dirty.insert(off, size);
        drop(s);
        if idle {
            self.shared.work.notify_one();
        }
        true
    }

    fn command<'a>(&'a self, mut s: Guard<'a>, cmd: u32) -> (Guard<'a>, u32) {
        let status = match cmd {
            CMD_ALLOCATE_BLOCK => {
                let size = Self::reg64(&s, REG_BLOCK_SIZE_LOW, REG_BLOCK_SIZE_HIGH);
                let got;
                (s, got) = self.allocate(s, size);
                match got {
                    Some((off, size)) => {
                        Self::set64(&mut s, REG_BLOCK_OFFSET_LOW, REG_BLOCK_OFFSET_HIGH, off);
                        Self::set64(&mut s, REG_BLOCK_SIZE_LOW, REG_BLOCK_SIZE_HIGH, size);
                        0
                    }
                    None => 12, // ENOMEM
                }
            }
            CMD_DEALLOCATE_BLOCK => {
                let off = Self::reg64(&s, REG_BLOCK_OFFSET_LOW, REG_BLOCK_OFFSET_HIGH);
                let freed = self.deallocate(s, off);
                s = self.shared.lock();
                if freed { 0 } else { 22 }
            }
            CMD_GEN_HANDLE => {
                let h = s.next_handle;
                s.next_handle += 1;
                s.handles.insert(h, (0, None));
                s.regs.insert(REG_HANDLE, h);
                0
            }
            CMD_DESTROY_HANDLE => {
                let h = Self::reg(&s, REG_HANDLE);
                s.handles.remove(&h);
                0
            }
            CMD_TELL_PING_INFO_ADDR => {
                let h = Self::reg(&s, REG_HANDLE);
                let addr = Self::reg64(&s, REG_PING_INFO_ADDR_LOW, REG_PING_INFO_ADDR_HIGH);
                if let Some(e) = s.handles.get_mut(&h) {
                    e.0 = addr;
                }
                0
            }
            _ => 22, // EINVAL
        };
        (s, status)
    }

    /// Ping info layout: offset u64 (guest phys), size u64, metadata u64, version u32,
    /// wait_fd u32, wait_flags u32, direction u32, data_size u64.
    fn ping<'a>(&'a self, mut s: Guard<'a>, handle: u32) {
        let Some(&(addr, kind)) = s.handles.get(&handle) else { return };
        if addr == 0 {
            return;
        }
        let metadata_addr = addr + 16;
        let metadata = self.mem.read_u64(metadata_addr).unwrap_or(u64::MAX);
        let result: u64 = match kind {
            None => {
                // First ping: choose the context type.
                if let Some(e) = s.handles.get_mut(&handle) {
                    e.1 = Some(metadata);
                }
                0
            }
            // Shared slots: the host picks the place. Allocate returns the block's offset in
            // the area (phys_addr) and its page-rounded size; the guest maps it from there.
            Some(ctx::SHARED_SLOTS_HOST_MEMORY_ALLOCATOR) => match metadata {
                hma::ALLOCATE => {
                    let size = self.mem.read_u64(addr + 8).unwrap_or(0);
                    match self.allocate(s, size).1 {
                        Some((off, size)) => {
                            let _ = self.mem.write(addr, &off.to_le_bytes());
                            let _ = self.mem.write(addr + 8, &size.to_le_bytes());
                            0
                        }
                        None => u64::MAX,
                    }
                }
                hma::UNALLOCATE => {
                    // Offsets are area-relative; accept an absolute address too.
                    let at = self.mem.read_u64(addr).unwrap_or(u64::MAX);
                    let off = if at >= self.area().base() { at - self.area().base() } else { at };
                    if self.deallocate(s, off) { 0 } else { u64::MAX }
                }
                hma::CHECK_SHARED_SLOTS => 0,
                _ => u64::MAX,
            },
            Some(ctx::HOST_MEMORY_ALLOCATOR) => {
                let phys = self.mem.read_u64(addr).unwrap_or(0);
                let size = self.mem.read_u64(addr + 8).unwrap_or(0);
                let area = self.area();
                let inside = phys >= area.base() && phys.saturating_add(size) <= area.end();
                match metadata {
                    // The guest names a block it allocated; fresh host memory reads as zero like
                    // stock's. The guest could have written there since, so clear it, with the
                    // lock released.
                    hma::ALLOCATE if inside => {
                        drop(s);
                        let _ = area.zero(phys, size as usize);
                        0
                    }
                    hma::UNALLOCATE if inside => 0,
                    _ => u64::MAX,
                }
            }
            // Other context types (graphics ASG, sensors, power...) aren't provided over this
            // transport; virtio-gpu carries graphics.
            Some(_) => u64::MAX,
        };
        let _ = self.mem.write(metadata_addr, &result.to_le_bytes());
    }
}

impl Drop for GoldfishAddressSpace {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.work.notify_all();
        if let Some(t) = self.clearer.take() {
            let _ = t.join();
        }
    }
}

impl MmioDevice for GoldfishAddressSpace {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let s = self.shared.lock();
        set_le(data, u64::from(Self::reg(&s, offset)));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = le_value(data) as u32;
        let mut s = self.shared.lock();
        match offset {
            REG_COMMAND => {
                let (mut s, status) = self.command(s, v);
                s.regs.insert(REG_STATUS, status);
            }
            REG_PING | REG_PING_WITH_DATA => self.ping(s, v),
            REG_GUEST_PAGE_SIZE => {
                s.page_size = u64::from(v.max(4096));
                s.regs.insert(offset, v);
            }
            REG_PHYS_START_LOW | REG_PHYS_START_HIGH => {
                s.regs.insert(offset, v);
            }
            _ => {
                s.regs.insert(offset, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: u64 = 0x4000_0000;
    const AREA: u64 = 0x8_0000_0000;

    fn wr(d: &GoldfishAddressSpace, r: u64, v: u32) {
        d.write(r, &v.to_le_bytes());
    }

    fn rd(d: &GoldfishAddressSpace, r: u64) -> u32 {
        let mut b = [0u8; 4];
        d.read(r, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn blocks_handles_and_allocator_pings() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        wr(&d, REG_BLOCK_SIZE_LOW, 5000);
        wr(&d, REG_COMMAND, CMD_ALLOCATE_BLOCK);
        assert_eq!(rd(&d, REG_STATUS), 0);
        assert_eq!(rd(&d, REG_BLOCK_OFFSET_LOW), 0);
        assert_eq!(rd(&d, REG_BLOCK_SIZE_LOW), 8192, "rounded to pages");
        wr(&d, REG_BLOCK_SIZE_LOW, 4096);
        wr(&d, REG_COMMAND, CMD_ALLOCATE_BLOCK);
        assert_eq!(rd(&d, REG_BLOCK_OFFSET_LOW), 8192);
        wr(&d, REG_BLOCK_OFFSET_LOW, 0);
        wr(&d, REG_COMMAND, CMD_DEALLOCATE_BLOCK);
        assert_eq!(rd(&d, REG_STATUS), 0);
        wr(&d, REG_COMMAND, CMD_DEALLOCATE_BLOCK);
        assert_ne!(rd(&d, REG_STATUS), 0, "double free");

        wr(&d, REG_COMMAND, CMD_GEN_HANDLE);
        let h = rd(&d, REG_HANDLE);
        let ping = RAM + 0x1000;
        wr(&d, REG_PING_INFO_ADDR_LOW, ping as u32);
        wr(&d, REG_PING_INFO_ADDR_HIGH, (ping >> 32) as u32);
        wr(&d, REG_COMMAND, CMD_TELL_PING_INFO_ADDR);
        assert_eq!(rd(&d, REG_PING_INFO_ADDR_LOW), ping as u32, "driver reads it back");
        // Select the host memory allocator context.
        mem.write(ping + 16, &ctx::HOST_MEMORY_ALLOCATOR.to_le_bytes()).expect("w");
        wr(&d, REG_PING, h);
        assert_eq!(mem.read_u64(ping + 16).expect("r"), 0);
        // Allocate host memory at a block inside the area.
        area.write(AREA + 8192, &[0xaa; 16]).expect("w");
        mem.write(ping, &(AREA + 8192).to_le_bytes()).expect("w");
        mem.write(ping + 8, &4096u64.to_le_bytes()).expect("w");
        mem.write(ping + 16, &hma::ALLOCATE.to_le_bytes()).expect("w");
        wr(&d, REG_PING, h);
        assert_eq!(mem.read_u64(ping + 16).expect("r"), 0);
        assert_eq!(area.read_u32(AREA + 8192).expect("r"), 0, "fresh memory is zeroed");
    }

    #[test]
    fn shared_slots_allocator_picks_offsets() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        wr(&d, REG_COMMAND, CMD_GEN_HANDLE);
        let h = rd(&d, REG_HANDLE);
        let ping = RAM + 0x2000;
        wr(&d, REG_PING_INFO_ADDR_LOW, ping as u32);
        wr(&d, REG_COMMAND, CMD_TELL_PING_INFO_ADDR);
        let call = |metadata: u64, phys: u64, size: u64| {
            mem.write(ping, &phys.to_le_bytes()).expect("w");
            mem.write(ping + 8, &size.to_le_bytes()).expect("w");
            mem.write(ping + 16, &metadata.to_le_bytes()).expect("w");
            wr(&d, REG_PING, h);
            (mem.read_u64(ping).expect("r"), mem.read_u64(ping + 8).expect("r"), mem.read_u64(ping + 16).expect("r"))
        };
        assert_eq!(call(ctx::SHARED_SLOTS_HOST_MEMORY_ALLOCATOR, 0, 0).2, 0);
        assert_eq!(call(hma::CHECK_SHARED_SLOTS, 0, 0).2, 0);
        assert_eq!(call(hma::ALLOCATE, 0, 100), (0, 4096, 0), "offset 0, one page");
        area.write(AREA, &[0xaa; 8]).expect("w");
        assert_eq!(call(hma::UNALLOCATE, 0, 0).2, 0);
        d.settle();
        assert_eq!(call(hma::ALLOCATE, 0, 100), (0, 4096, 0), "the same page again");
        assert_eq!(area.read_u32(AREA).expect("r"), 0, "zeroed");
        assert_eq!(call(hma::ALLOCATE, 0, 5000), (4096, 8192, 0));
        assert_eq!(call(hma::UNALLOCATE, 0, 0).2, 0);
        assert_eq!(call(hma::UNALLOCATE, AREA + 4096, 0).2, 0, "absolute address");
        assert_eq!(call(hma::UNALLOCATE, 0, 0).2, u64::MAX, "double free");
    }

    /// A shared-slots allocator handle whose ping info lives at `ping`.
    struct Slots<'a> {
        d: &'a GoldfishAddressSpace,
        mem: &'a GuestRam,
        h: u32,
        ping: u64,
    }

    impl Slots<'_> {
        fn open<'a>(d: &'a GoldfishAddressSpace, mem: &'a GuestRam, ping: u64) -> Slots<'a> {
            wr(d, REG_COMMAND, CMD_GEN_HANDLE);
            let h = rd(d, REG_HANDLE);
            wr(d, REG_PING_INFO_ADDR_LOW, ping as u32);
            wr(d, REG_PING_INFO_ADDR_HIGH, (ping >> 32) as u32);
            wr(d, REG_COMMAND, CMD_TELL_PING_INFO_ADDR);
            let s = Slots { d, mem, h, ping };
            assert_eq!(s.call(ctx::SHARED_SLOTS_HOST_MEMORY_ALLOCATOR, 0, 0).2, 0);
            s
        }

        fn call(&self, metadata: u64, phys: u64, size: u64) -> (u64, u64, u64) {
            let (mem, ping) = (self.mem, self.ping);
            mem.write(ping, &phys.to_le_bytes()).expect("w");
            mem.write(ping + 8, &size.to_le_bytes()).expect("w");
            mem.write(ping + 16, &metadata.to_le_bytes()).expect("w");
            wr(self.d, REG_PING, self.h);
            (mem.read_u64(ping).expect("r"), mem.read_u64(ping + 8).expect("r"), mem.read_u64(ping + 16).expect("r"))
        }

        fn alloc(&self, size: u64) -> Option<(u64, u64)> {
            let (off, size, r) = self.call(hma::ALLOCATE, 0, size);
            (r == 0).then_some((off, size))
        }

        fn free(&self, off: u64) -> bool {
            self.call(hma::UNALLOCATE, off, 0).2 == 0
        }
    }

    fn block(d: &GoldfishAddressSpace, size: u64) -> Option<u64> {
        wr(d, REG_BLOCK_SIZE_LOW, size as u32);
        wr(d, REG_BLOCK_SIZE_HIGH, (size >> 32) as u32);
        wr(d, REG_COMMAND, CMD_ALLOCATE_BLOCK);
        (rd(d, REG_STATUS) == 0).then(|| u64::from(rd(d, REG_BLOCK_OFFSET_LOW)) | (u64::from(rd(d, REG_BLOCK_OFFSET_HIGH)) << 32))
    }

    fn unblock(d: &GoldfishAddressSpace, off: u64) -> bool {
        wr(d, REG_BLOCK_OFFSET_LOW, off as u32);
        wr(d, REG_BLOCK_OFFSET_HIGH, (off >> 32) as u32);
        wr(d, REG_COMMAND, CMD_DEALLOCATE_BLOCK);
        rd(d, REG_STATUS) == 0
    }

    /// Allocates, fills and frees `n` blocks of `size`, then lets the clearing thread finish:
    /// returns the area's resident bytes with the blocks full and after they were cleared. Unix
    /// only: `GuestRam::resident` has no Windows reading yet.
    #[cfg(unix)]
    fn churn(clearer: bool, n: u64, size: u64) -> (u64, u64, Arc<GuestRam>) {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, n * size + (1 << 20)).expect("area"));
        let d = GoldfishAddressSpace::build(mem, area.clone(), clearer);
        let blocks: Vec<u64> = (0..n).map(|_| block(&d, size).expect("block")).collect();
        for &off in &blocks {
            area.write(AREA + off, &vec![0x5a; size as usize]).expect("fill");
        }
        let full = area.resident(AREA, area.len()).expect("resident");
        for &off in &blocks {
            assert!(unblock(&d, off));
        }
        d.settle();
        let cleared = area.resident(AREA, area.len()).expect("resident");
        (full, cleared, area)
    }

    #[cfg(unix)]
    #[test]
    fn freed_runs_go_back_to_the_host() {
        // 16 gralloc-sized 4 MiB buffers, freed: the clearer releases their pages.
        let (full, cleared, area) = churn(true, 16, 4 << 20);
        assert!(full >= 64 << 20, "filled blocks are resident: {full}");
        assert_eq!(cleared, 0, "cleared runs went back to the host");
        assert!(reads_zero(&area, 0, 64 << 20), "and read as zero");
    }

    #[cfg(unix)]
    #[test]
    fn short_runs_and_partial_pages_are_zeroed_in_place() {
        // One freed run shorter than DISCARD_MIN: zeroed with stores, so its pages stay.
        let (full, cleared, area) = churn(true, 1, 0x8_0000);
        assert!(full >= 0x8_0000);
        assert_eq!(cleared, full, "nothing discarded");
        assert!(reads_zero(&area, 0, 0x8_0000));
        // A long run that starts and ends inside host pages: those partial pages are zeroed and
        // stay resident, the whole pages between go back.
        let a = GuestRam::new(AREA, 4 << 20).expect("area");
        a.write(AREA, &vec![0x77; 4 << 20]).expect("fill");
        clear(&a, 0x1000, (2 << 20) + 0x1000);
        // Measured before reading the run back, which commits pages again.
        let resident = a.resident(AREA, 4 << 20).expect("resident");
        assert!(resident <= (2 << 20) + 2 * DISCARD_GRANULE, "inner pages released: {resident:#x}");
        assert!(reads_zero(&a, 0x1000, (2 << 20) + 0x1000));
        let mut edge = [0u8; 2];
        a.read(AREA + 0xfff, &mut edge[..1]).expect("r");
        a.read(AREA + (2 << 20) + 0x2000, &mut edge[1..]).expect("r");
        assert_eq!(edge, [0x77, 0x77], "bytes outside the run untouched");
    }

    #[test]
    fn a_run_left_unmapped_from_the_guest_is_never_reused() {
        let a = GuestRam::new(AREA, 4 << 20).expect("area");
        a.write(AREA, &vec![0x77; 4 << 20]).expect("fill");
        fn hole(_: &GuestRam, ipa: u64, len: u64) -> emu_hvf::Result<u64> {
            Err(emu_hvf::Error::GuestHole { ipa, len: len as usize, code: -1 })
        }
        fn other(_: &GuestRam, _: u64, _: u64) -> emu_hvf::Result<u64> {
            Err(emu_hvf::Error::Mmap(0))
        }
        assert!(!clear_with(&a, 0, 2 << 20, hole), "leaked");
        assert!(clear_with(&a, 2 << 20, 2 << 20, other), "any other failure: zeroed in place");
        assert!(reads_zero(&a, 2 << 20, 2 << 20));
        // In the device: the clearing thread leaves the run out of the allocator for good.
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 4 << 20).expect("area"));
        let d = GoldfishAddressSpace::build_with(mem, area, true, hole);
        let off = block(&d, 2 << 20).expect("block");
        assert_eq!(off, 0);
        assert!(unblock(&d, off));
        d.settle();
        let s = d.shared.lock();
        assert_eq!(s.clean.by_off, BTreeMap::from([(2 << 20, 2 << 20)]), "only the untouched half");
        assert!(s.dirty.is_empty() && s.clearing == 0);
        drop(s);
        assert_eq!(block(&d, 3 << 20), None, "the hole is never handed out");
        assert_eq!(block(&d, 2 << 20), Some(2 << 20));
    }

    /// Before/after numbers: host memory a freed 64 MiB of gralloc buffers keeps, zeroed in place
    /// (the old clearer) or cleared with `clear`.
    #[test]
    #[ignore = "bench: prints numbers"]
    fn clear_cost() {
        for (name, discard) in [("zero in place", false), ("clear (discard)", true)] {
            let a = GuestRam::new(AREA, 64 << 20).expect("area");
            a.write(AREA, &vec![0x5a; 64 << 20]).expect("fill");
            let t = std::time::Instant::now();
            for off in (0..64u64 << 20).step_by(CLEAR_CHUNK as usize) {
                if discard {
                    clear(&a, off, CLEAR_CHUNK);
                } else {
                    a.zero(AREA + off, CLEAR_CHUNK as usize).expect("zero");
                }
            }
            let took = t.elapsed();
            let resident = a.resident(AREA, 64 << 20).expect("resident");
            println!("{name}: 64 MiB in {took:?}, {} MiB still resident", resident >> 20);
        }
    }

    fn reads_zero(area: &GuestRam, off: u64, len: u64) -> bool {
        let mut b = vec![0xff; len as usize];
        area.read(area.base() + off, &mut b).expect("r");
        b.iter().all(|&x| x == 0)
    }

    #[test]
    fn extents_take_the_best_fit_and_merge_neighbours() {
        let mut e = Extents::default();
        e.insert(0x8000, 0x2000);
        e.insert(0x3000, 0x3000);
        e.insert(0x1000, 0x1000);
        e.insert(0x7000 - 0x7000, 0x1000);
        assert_eq!(e.by_off, BTreeMap::from([(0, 0x2000), (0x3000, 0x3000), (0x8000, 0x2000)]), "merged on the right");
        e.insert(0x2000, 0x1000);
        assert_eq!(e.by_off, BTreeMap::from([(0, 0x6000), (0x8000, 0x2000)]), "merged on both sides");
        assert_eq!(e.take(0x1000), Some(0x8000), "the smallest extent that fits");
        assert_eq!(e.take(0x1000), Some(0x9000), "exact fit");
        assert_eq!(e.take(0x7000), None);
        e.insert(0x8000, 0x2000);
        assert_eq!(e.take(0x2000), Some(0x8000), "the lowest of equals");
        assert_eq!(e.take(0x6000), Some(0));
        assert!(e.is_empty() && e.by_size.is_empty());
        e.insert(0x4000, 0x5000);
        assert_eq!(e.take_front(0x2000), Some((0x4000, 0x2000)));
        assert_eq!(e.take_front(0x10000), Some((0x6000, 0x3000)), "what's left");
        assert_eq!(e.take_front(1), None);
        assert!(e.by_size.is_empty(), "both indexes agree");
    }

    #[test]
    fn reused_blocks_read_as_zero() {
        // With the clearing thread racing the allocation, and without it (the allocation
        // clears the dirty block itself).
        for clearer in [false, true] {
            let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
            let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
            let d = GoldfishAddressSpace::build(mem.clone(), area.clone(), clearer);
            let a = Slots::open(&d, &mem, RAM + 0x1000);
            for round in 0..3u8 {
                let (off, size) = a.alloc(0x10_0000).expect("the whole area");
                assert_eq!((off, size), (0, 0x10_0000));
                assert!(reads_zero(&area, off, size), "round {round}, clearer {clearer}");
                area.write(AREA + off, &vec![0xa0 | round; size as usize]).expect("w");
                assert!(a.free(off));
            }
            // Block commands share the space and the guarantee.
            let off = block(&d, 0x8_0000).expect("block");
            assert!(reads_zero(&area, off, 0x8_0000), "clearer {clearer}");
            d.settle();
            let s = d.shared.lock();
            assert!(s.dirty.is_empty() && s.clearing == 0);
            assert_eq!(s.clean.by_off, BTreeMap::from([(0x8_0000, 0x8_0000)]));
        }
    }

    #[test]
    fn allocation_clears_and_merges_when_nothing_fits() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x4000).expect("area"));
        let d = GoldfishAddressSpace::build(mem, area.clone(), false);
        assert_eq!(block(&d, 0x2000), Some(0));
        area.write(AREA, &[0xaa; 0x2000]).expect("w");
        assert!(unblock(&d, 0));
        assert!(!unblock(&d, 0), "double free");
        // Clean [0x2000, 0x4000) and dirty [0, 0x2000): neither holds three pages, both do.
        assert_eq!(block(&d, 0x3000), Some(0));
        assert!(reads_zero(&area, 0, 0x3000));
        assert!(d.shared.lock().dirty.is_empty(), "cleared while merging");
        assert_eq!(block(&d, 0x2000), None, "one page left");
        assert_eq!(block(&d, 0x1000), Some(0x3000));
    }

    #[test]
    fn allocation_waits_for_a_chunk_being_cleared() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x4000).expect("area"));
        let d = GoldfishAddressSpace::build(mem, area.clone(), false);
        assert_eq!(block(&d, 0x4000), Some(0));
        area.write(AREA, &[0xaa; 0x4000]).expect("w");
        assert!(unblock(&d, 0));
        // Stand in for the clearing thread halfway through the only free chunk.
        let (off, len) = {
            let mut s = d.shared.lock();
            let chunk = s.dirty.take_front(CLEAR_CHUNK).expect("dirty");
            s.clearing += chunk.1;
            chunk
        };
        std::thread::scope(|sc| {
            sc.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                area.zero(AREA + off, len as usize).expect("zero");
                let mut s = d.shared.lock();
                s.clearing -= len;
                s.clean.insert(off, len);
                d.shared.cleared.notify_all();
            });
            assert_eq!(block(&d, 0x4000), Some(0), "waited for the chunk");
        });
        assert!(reads_zero(&area, 0, 0x4000));
        assert!(unblock(&d, 0));
        d.settle();
        assert_eq!(block(&d, 0x8000), None, "larger than the area, with nothing left to clear");
    }

    #[test]
    fn registers_handles_and_unknown_requests() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area);
        wr(&d, REG_GUEST_PAGE_SIZE, 0x4000);
        assert_eq!(rd(&d, REG_GUEST_PAGE_SIZE), 0x4000);
        assert_eq!(block(&d, 1), Some(0));
        assert_eq!(rd(&d, REG_BLOCK_SIZE_LOW), 0x4000, "rounded to the guest's pages");
        wr(&d, REG_PHYS_START_LOW, 0x1234);
        assert_eq!(rd(&d, REG_PHYS_START_LOW), 0x1234);
        wr(&d, REG_COMMAND, 99);
        assert_eq!(rd(&d, REG_STATUS), 22, "unknown command");
        // A handle with no ping info ignores pings.
        wr(&d, REG_COMMAND, CMD_GEN_HANDLE);
        wr(&d, REG_PING, rd(&d, REG_HANDLE));
        let a = Slots::open(&d, &mem, RAM + 0x1000);
        assert_eq!(a.call(42, 0, 0).2, u64::MAX, "unknown allocator command");
        let b = Slots::open(&d, &mem, RAM + 0x2000);
        let other = |s: &Slots, ctx: u64, metadata: u64| {
            wr(s.d, REG_HANDLE, s.h);
            wr(s.d, REG_COMMAND, CMD_DESTROY_HANDLE);
            wr(s.d, REG_COMMAND, CMD_GEN_HANDLE);
            let s = Slots { h: rd(s.d, REG_HANDLE), ..*s };
            wr(s.d, REG_PING_INFO_ADDR_LOW, s.ping as u32);
            wr(s.d, REG_PING_INFO_ADDR_HIGH, 0);
            wr(s.d, REG_COMMAND, CMD_TELL_PING_INFO_ADDR);
            assert_eq!(s.call(ctx, 0, 0).2, 0);
            s.call(metadata, AREA, 0x1000).2
        };
        assert_eq!(other(&b, 1, hma::ALLOCATE), u64::MAX, "graphics and other contexts aren't served here");
        assert_eq!(other(&b, ctx::HOST_MEMORY_ALLOCATOR, hma::UNALLOCATE), 0);
        assert_eq!(other(&b, ctx::HOST_MEMORY_ALLOCATOR, 42), u64::MAX);
        // Destroyed handles ignore pings: the result slot keeps what the guest wrote.
        wr(&d, REG_HANDLE, a.h);
        wr(&d, REG_COMMAND, CMD_DESTROY_HANDLE);
        assert_eq!(a.call(hma::ALLOCATE, 0, 0x1000).2, hma::ALLOCATE);
    }

    #[test]
    fn best_fit_reuses_holes_before_untouched_space() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        let a = Slots::open(&d, &mem, RAM + 0x1000);
        // 64 blocks of 1, 2, 3, 4 pages...: 160 pages, back to back.
        let blocks: Vec<(u64, u64)> = (0..64u64).map(|i| a.alloc((i % 4 + 1) * 0x1000).expect("alloc")).collect();
        for w in blocks.windows(2) {
            assert_eq!(w[0].0 + w[0].1, w[1].0, "packed, no overlap");
        }
        let end = blocks[63].0 + blocks[63].1;
        assert_eq!(end, 160 * 0x1000);
        for &(off, size) in blocks.iter().step_by(2) {
            area.write(AREA + off, &vec![0x55; size as usize]).expect("w");
            assert!(a.free(off));
        }
        d.settle();
        // Holes of one and three pages between live blocks.
        assert_eq!(a.alloc(0x1000), Some((0, 0x1000)), "the first one-page hole");
        assert_eq!(a.alloc(0x3000), Some((0x3000, 0x3000)), "the first three-page hole");
        assert_eq!(a.alloc(0x4000), Some((end, 0x4000)), "no hole fits: the untouched tail");
        for &(off, size) in &[(0, 0x1000), (0x3000, 0x3000)] {
            assert!(reads_zero(&area, off, size));
        }
        // Everything back: one clean extent again, and the whole area fits.
        for &(off, _) in blocks.iter().skip(1).step_by(2) {
            assert!(a.free(off));
        }
        for off in [0, 0x3000, end] {
            assert!(a.free(off));
        }
        d.settle();
        assert_eq!(d.shared.lock().clean.by_off, BTreeMap::from([(0, 0x10_0000)]));
        assert_eq!(a.alloc(0x10_0000), Some((0, 0x10_0000)));
        assert!(reads_zero(&area, 0, 0x10_0000));
    }

    #[test]
    fn oversized_requests_fail_cleanly() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area);
        assert_eq!(block(&d, u64::MAX), None, "page rounding would overflow");
        let a = Slots::open(&d, &mem, RAM + 0x1000);
        assert_eq!(a.alloc(u64::MAX), None);
        assert_eq!(a.alloc(0x10_0001), None, "larger than the area");
        assert_eq!(a.alloc(0x10_0000), Some((0, 0x10_0000)), "nothing leaked");
    }

    #[test]
    fn concurrent_allocate_and_free_never_overlap() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 16 << 20).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        let live = Mutex::new(BTreeMap::<u64, u64>::new());
        // Opening a handle is a sequence over shared registers, which the guest driver runs
        // under its own lock; pings are single writes and race freely.
        let registers = Mutex::new(());
        std::thread::scope(|sc| {
            for t in 0..4u64 {
                let (d, mem, area, live, registers) = (&d, &mem, &area, &live, &registers);
                sc.spawn(move || {
                    let a = {
                        let _g = registers.lock().expect("registers");
                        Slots::open(d, mem, RAM + 0x1000 * (t + 1))
                    };
                    let mut held = std::collections::VecDeque::new();
                    for i in 0..300u64 {
                        let (off, size) = a.alloc(((i * 7 + t) % 8 + 1) * 0x2000).expect("alloc");
                        assert!(reads_zero(area, off, size), "thread {t} round {i}: fresh block at {off:#x}");
                        {
                            let mut l = live.lock().expect("live");
                            if let Some((&o, &s)) = l.range(..off + size).next_back() {
                                assert!(o + s <= off, "{off:#x}+{size:#x} overlaps {o:#x}+{s:#x}");
                            }
                            l.insert(off, size);
                        }
                        area.write(AREA + off, &vec![t as u8 + 1; size as usize]).expect("w");
                        held.push_back((off, size));
                        if held.len() > 3 {
                            let (off, size) = held.pop_front().expect("held");
                            let mut b = vec![0; size as usize];
                            area.read(AREA + off, &mut b).expect("r");
                            assert!(b.iter().all(|&x| x == t as u8 + 1), "block {off:#x} changed while held");
                            live.lock().expect("live").remove(&off);
                            assert!(a.free(off));
                        }
                    }
                });
            }
        });
        d.settle();
        let s = d.shared.lock();
        assert_eq!(s.used.len(), 12, "each thread still holds three");
        let free: u64 = s.clean.by_off.values().sum();
        assert_eq!(free + s.used.values().sum::<u64>(), area.len(), "no space lost");
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    /// Shared-slot ALLOCATE latency (the vCPU exit) for a 1080x2400 RGBA buffer, and lookup cost
    /// with 2048 holes in front of the first fit.
    ///     cargo test --release -p simee-emu-vmm allocate_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn allocate_cost() {
        use std::time::Instant;
        let buf = 1080 * 2400 * 4;
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 256 << 20).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        wr(&d, REG_COMMAND, CMD_GEN_HANDLE);
        let h = rd(&d, REG_HANDLE);
        let ping = RAM + 0x2000;
        wr(&d, REG_PING_INFO_ADDR_LOW, ping as u32);
        wr(&d, REG_COMMAND, CMD_TELL_PING_INFO_ADDR);
        let call = |metadata: u64, phys: u64, size: u64| {
            mem.write(ping, &phys.to_le_bytes()).expect("w");
            mem.write(ping + 8, &size.to_le_bytes()).expect("w");
            mem.write(ping + 16, &metadata.to_le_bytes()).expect("w");
            let t = Instant::now();
            wr(&d, REG_PING, h);
            let us = t.elapsed().as_secs_f64() * 1e6;
            assert_eq!(mem.read_u64(ping + 16).expect("r"), 0);
            (mem.read_u64(ping).expect("r"), us)
        };
        call(ctx::SHARED_SLOTS_HOST_MEMORY_ALLOCATOR, 0, 0);
        // First touch: the block's pages have never been committed.
        let (mut fresh, mut reuse, mut free) = (vec![], vec![], vec![]);
        for _ in 0..9 {
            let (off, us) = call(hma::ALLOCATE, 0, buf);
            fresh.push(us);
            // Steady state: the guest draws into it, frees it, and allocates the same size again.
            area.write(AREA + off, &vec![0x5a; buf as usize]).expect("w");
            free.push(call(hma::UNALLOCATE, off, 0).1);
            d.settle();
            let (off, us) = call(hma::ALLOCATE, 0, buf);
            reuse.push(us);
            area.write(AREA + off, &vec![0x5a; buf as usize]).expect("w");
            call(hma::UNALLOCATE, off, 0);
            d.settle();
            // Leave the touched pages behind and start the next round on untouched ones.
            let (keep, _) = call(hma::ALLOCATE, 0, 2 * buf);
            assert!(keep < area.len());
        }
        println!("allocate 1080x2400x4, untouched pages: {:.1} us", median(fresh));
        println!("allocate 1080x2400x4, reused block:    {:.1} us", median(reuse));
        println!("unallocate 1080x2400x4:                 {:.1} us", median(free));

        // Worst case: freed and asked for again at once with no clean space that fits, so the
        // allocation clears (or waits for) the dirty block itself.
        let area = Arc::new(GuestRam::new(AREA, buf.div_ceil(0x4000) * 0x4000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        let a = Slots::open(&d, &mem, RAM + 0x3000);
        let mut lag = vec![];
        for _ in 0..9 {
            let (off, size) = a.alloc(buf).expect("alloc");
            area.write(AREA + off, &vec![0x5a; size as usize]).expect("w");
            assert!(a.free(off));
            let t = Instant::now();
            let (off, size) = a.alloc(buf).expect("realloc");
            lag.push(t.elapsed().as_secs_f64() * 1e6);
            assert!(reads_zero(&area, off, size));
            assert!(a.free(off));
            d.settle();
        }
        println!("allocate 1080x2400x4, nothing clean:   {:.1} us", median(lag));

        // Fragmentation: 2048 one-page holes, then two-page requests.
        let area = Arc::new(GuestRam::new(AREA, 256 << 20).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area);
        let blocks: Vec<u64> = (0..4096)
            .map(|_| {
                wr(&d, REG_BLOCK_SIZE_LOW, 4096);
                wr(&d, REG_BLOCK_SIZE_HIGH, 0);
                wr(&d, REG_COMMAND, CMD_ALLOCATE_BLOCK);
                u64::from(rd(&d, REG_BLOCK_OFFSET_LOW))
            })
            .collect();
        for off in blocks.iter().step_by(2) {
            wr(&d, REG_BLOCK_OFFSET_LOW, *off as u32);
            wr(&d, REG_COMMAND, CMD_DEALLOCATE_BLOCK);
        }
        d.settle();
        let mut holes = vec![];
        for _ in 0..15 {
            wr(&d, REG_BLOCK_SIZE_LOW, 8192);
            let t = Instant::now();
            wr(&d, REG_COMMAND, CMD_ALLOCATE_BLOCK);
            holes.push(t.elapsed().as_secs_f64() * 1e6);
            assert_eq!(rd(&d, REG_STATUS), 0);
        }
        println!("allocate block past 2048 holes:         {:.2} us", median(holes));
    }
}
