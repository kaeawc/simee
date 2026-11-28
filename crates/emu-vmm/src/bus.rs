//! MMIO dispatch: guest physical address ranges → devices.
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// A memory-mapped device. Offsets are relative to the device's base address. Accesses are
/// 1, 2, 4 or 8 bytes, little-endian.
pub trait MmioDevice: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

struct Range {
    base: u64,
    len: u64,
    dev: Arc<dyn MmioDevice>,
}

/// Immutable after construction, so vCPU threads read it without locking.
#[derive(Default)]
pub struct Bus {
    ranges: Vec<Range>,
}

impl Bus {
    pub fn new() -> Bus {
        Bus::default()
    }

    /// Registers `dev` at `[base, base + len)`. Panics on overlap: the memory map is static.
    pub fn insert(&mut self, base: u64, len: u64, dev: Arc<dyn MmioDevice>) {
        assert!(
            self.ranges.iter().all(|r| base + len <= r.base || r.base + r.len <= base),
            "MMIO range {base:#x}+{len:#x} overlaps an existing device"
        );
        self.ranges.push(Range { base, len, dev });
        self.ranges.sort_by_key(|r| r.base);
    }

    /// Returns false when nothing is mapped at `addr`.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        self.read_at(addr, data).is_some()
    }

    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.write_at(addr, data).is_some()
    }

    /// Like [`Bus::read`], returning which range took the access (its index in base order, as
    /// [`ExitProfile`] counts them) and the offset within it; `None` when nothing is mapped.
    pub fn read_at(&self, addr: u64, data: &mut [u8]) -> Option<(usize, u64)> {
        let i = self.index(addr)?;
        let r = &self.ranges[i];
        r.dev.read(addr - r.base, data);
        Some((i, addr - r.base))
    }

    pub fn write_at(&self, addr: u64, data: &[u8]) -> Option<(usize, u64)> {
        let i = self.index(addr)?;
        let r = &self.ranges[i];
        r.dev.write(addr - r.base, data);
        Some((i, addr - r.base))
    }

    fn index(&self, addr: u64) -> Option<usize> {
        let i = self.ranges.partition_point(|r| r.base <= addr).checked_sub(1)?;
        (addr < self.ranges[i].base + self.ranges[i].len).then_some(i)
    }

    /// Drops the devices one at a time, in base order, calling `before(name)` before each, so a
    /// teardown that overruns can say whose release it was waiting for.
    pub fn release_each(self, mut before: impl FnMut(&str)) {
        for r in self.ranges {
            before(&region_name(r.base));
            drop(r.dev);
        }
    }

    /// `(base, len)` of every range, in the order [`Bus::read_at`] indexes them.
    pub fn regions(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges.iter().map(|r| (r.base, r.len))
    }
}

/// What `EMU_EXIT_PROFILE` asks for: unset, empty or `0` is off; `1` prints the profile to
/// stderr when the machine stops; anything else also names a file the profile is rewritten to
/// every second (diff two snapshots to profile a phase of a running guest).
pub fn profile_target(var: Option<&str>) -> Option<Option<PathBuf>> {
    match var {
        None | Some("" | "0") => None,
        Some("1") => Some(None),
        Some(path) => Some(Some(PathBuf::from(path))),
    }
}

/// Per-register buckets per region: the 4-byte registers of its first 4 KiB, the last bucket
/// collecting everything above (PCI config space, the pipe's second page).
pub const PROFILE_BUCKETS: usize = 1024;

/// Which device and register each MMIO exit hit, and where each vCPU's time went: enabled with
/// `EMU_EXIT_PROFILE` (see [`profile_target`]). Counters are relaxed atomics in arrays sized at
/// start, so recording an exit neither allocates nor locks. vCPUs share the region counters:
/// profiling adds cache-line traffic between them, which is why it is off by default.
pub struct ExitProfile {
    /// One per bus range, in base order, then one for unmapped addresses.
    regions: Box<[RegionCounters]>,
    cpus: Box<[CpuTimes]>,
    started: Instant,
    /// Nanoseconds from `started` to the first guest entry, plus one (zero: not entered yet).
    first_entry: AtomicU64,
}

struct RegionCounters {
    name: String,
    base: u64,
    reads: AtomicU64,
    writes: AtomicU64,
    read_ns: AtomicU64,
    write_ns: AtomicU64,
    /// `[read, write]` counts per 4-byte register.
    regs: Box<[[AtomicU64; 2]]>,
}

/// Where one vCPU thread's time went, in nanoseconds. Its own cache line, like `ExitStats`.
#[derive(Default)]
#[repr(align(128))]
pub struct CpuTimes {
    /// Calls into the guest (`hv_vcpu_run`), and the time inside them. With the in-kernel GIC
    /// the framework handles WFI itself, so this includes the guest idling: `sample` tells the
    /// two apart (`hv_trap` runs the guest, `wait_for_interrupt` waits).
    pub runs: AtomicU64,
    pub guest_ns: AtomicU64,
    /// Parked in host-side WFI / PSCI CPU_SUSPEND (rare: see `guest_ns`).
    pub parked_ns: AtomicU64,
    /// In MMIO device handlers.
    pub mmio_ns: AtomicU64,
}

impl ExitProfile {
    pub fn new(bus: &Bus, cpus: usize) -> ExitProfile {
        let region = |name: String, base: u64| RegionCounters {
            name,
            base,
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            read_ns: AtomicU64::new(0),
            write_ns: AtomicU64::new(0),
            regs: (0..PROFILE_BUCKETS).map(|_| [AtomicU64::new(0), AtomicU64::new(0)]).collect(),
        };
        let mut regions: Vec<_> = bus.regions().map(|(base, _)| region(region_name(base), base)).collect();
        regions.push(region("unmapped".into(), 0));
        ExitProfile {
            regions: regions.into(),
            cpus: (0..cpus).map(|_| CpuTimes::default()).collect(),
            started: Instant::now(),
            first_entry: AtomicU64::new(0),
        }
    }

    /// One MMIO exit: `hit` is what [`Bus::read_at`] / [`Bus::write_at`] returned, `ns` the time
    /// in the handler.
    pub fn record(&self, cpu: usize, hit: Option<(usize, u64)>, write: bool, ns: u64) {
        let (i, offset) = hit.unwrap_or((self.regions.len() - 1, 0));
        let Some(r) = self.regions.get(i) else { return };
        let (count, time) = if write { (&r.writes, &r.write_ns) } else { (&r.reads, &r.read_ns) };
        count.fetch_add(1, Ordering::Relaxed);
        time.fetch_add(ns, Ordering::Relaxed);
        let bucket = ((offset >> 2) as usize).min(PROFILE_BUCKETS - 1);
        r.regs[bucket][usize::from(write)].fetch_add(1, Ordering::Relaxed);
        if let Some(c) = self.cpus.get(cpu) {
            c.mmio_ns.fetch_add(ns, Ordering::Relaxed);
        }
    }

    pub fn cpu(&self, idx: usize) -> &CpuTimes {
        &self.cpus[idx]
    }

    /// Marks a guest entry; the first one is reported as the time from setup to the guest.
    pub fn entered(&self) {
        if self.first_entry.load(Ordering::Relaxed) == 0 {
            let ns = self.started.elapsed().as_nanos() as u64 + 1;
            let _ = self.first_entry.compare_exchange(0, ns, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    /// The whole profile, one record per line, for scripts to diff:
    /// `T <ns since vCPU setup> <ns to first guest entry, 0 if none> <unix ms now>`,
    /// `C <vcpu> <runs> <guest_ns> <parked_ns> <mmio_ns>`,
    /// `R <region> <base> <reads> <writes> <read_ns> <write_ns>` and, for every register hit,
    /// `O <region> <offset> <reads> <writes>`.
    pub fn dump(&self) -> String {
        use std::fmt::Write;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let first = l(&self.first_entry).saturating_sub(1);
        let mut s = format!("T {} {first} {unix_ms}\n", self.started.elapsed().as_nanos());
        for (i, c) in self.cpus.iter().enumerate() {
            let _ = writeln!(s, "C {i} {} {} {} {}", l(&c.runs), l(&c.guest_ns), l(&c.parked_ns), l(&c.mmio_ns));
        }
        for r in self.regions.iter().filter(|r| l(&r.reads) + l(&r.writes) > 0) {
            let _ = writeln!(s, "R {} {:#x} {} {} {} {}", r.name, r.base, l(&r.reads), l(&r.writes), l(&r.read_ns), l(&r.write_ns));
            for (b, [rd, wr]) in r.regs.iter().enumerate().filter(|(_, c)| l(&c[0]) + l(&c[1]) > 0) {
                let _ = writeln!(s, "O {} {:#x} {} {}", r.name, b * 4, l(rd), l(wr));
            }
        }
        s
    }

    /// For people: per-vCPU time split, then regions by exit count with their top registers.
    pub fn summary(&self) -> String {
        use std::fmt::Write;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let secs = |ns: u64| ns as f64 / 1e9;
        let wall = self.started.elapsed().as_nanos() as u64;
        let first = l(&self.first_entry);
        let mut s = format!(
            "emu: exit profile over {:.1}s; first guest entry {:.3}s after vCPU setup\n",
            secs(wall),
            if first == 0 { f64::NAN } else { secs(first - 1) }
        );
        for (i, c) in self.cpus.iter().enumerate() {
            let _ = writeln!(
                s,
                "emu:   vcpu{i}: {} runs, in guest {:.2}s, parked {:.2}s, in MMIO handlers {:.2}s",
                l(&c.runs),
                secs(l(&c.guest_ns)),
                secs(l(&c.parked_ns)),
                secs(l(&c.mmio_ns))
            );
        }
        let mut regions: Vec<_> = self.regions.iter().filter(|r| l(&r.reads) + l(&r.writes) > 0).collect();
        regions.sort_by_key(|r| std::cmp::Reverse(l(&r.reads) + l(&r.writes)));
        for r in regions {
            let (rd, wr) = (l(&r.reads), l(&r.writes));
            let mut regs: Vec<_> = r.regs.iter().enumerate().map(|(b, c)| (b * 4, l(&c[0]), l(&c[1]))).filter(|c| c.1 + c.2 > 0).collect();
            regs.sort_by_key(|c| std::cmp::Reverse(c.1 + c.2));
            let top: Vec<String> =
                regs.iter().take(6).map(|&(o, rd, wr)| format!("{}={rd}r/{wr}w", register_name(&r.name, o as u64))).collect();
            let per = |n: u64, ns: u64| if n == 0 { 0.0 } else { ns as f64 / n as f64 / 1e3 };
            let _ = writeln!(
                s,
                "emu:   {:<16} {rd:>9} reads ({:.1}us) {wr:>9} writes ({:.1}us)  {}",
                r.name,
                per(rd, l(&r.read_ns)),
                per(wr, l(&r.write_ns)),
                top.join(" ")
            );
        }
        s
    }
}

/// The device at `base` on the stock board (`layout.rs`).
pub fn region_name(base: u64) -> String {
    use crate::layout::*;
    let fixed = match base {
        UART_BASE => "pl011",
        RTC_BASE => "pl031",
        GOLDFISH_BATTERY_BASE => "battery",
        GOLDFISH_PIPE_BASE => "goldfish-pipe",
        PCI_ECAM_BASE => "pci-ecam",
        GOLDFISH_AS_REGS => "goldfish-as",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    let end = virtio_slot_addr(VIRTIO_MMIO_SLOTS);
    if (VIRTIO_MMIO_BASE..end).contains(&base) && (base - VIRTIO_MMIO_BASE).is_multiple_of(VIRTIO_MMIO_STRIDE) {
        let slot = ((base - VIRTIO_MMIO_BASE) / VIRTIO_MMIO_STRIDE) as u32;
        use slots::*;
        let name = match slot {
            GPU => "gpu",
            TOUCH => "touch",
            KEYBOARD => "keyboard",
            ROTARY => "rotary",
            MOUSE => "mouse",
            TOUCHPAD => "touchpad",
            VSOCK => "vsock",
            RNG => "rng",
            NET => "net",
            SEGMENT => "net-segment",
            VPORTS => "vports",
            SOUND => "snd",
            SYSTEM => "blk-system",
            CACHE => "blk-cache",
            USERDATA => "blk-userdata",
            METADATA => "blk-metadata",
            VENDOR => "blk-vendor",
            _ => return format!("virtio{slot}"),
        };
        return format!("virtio-{name}");
    }
    format!("mmio@{base:#x}")
}

/// A register's name where the region's register map is well known, else its offset.
pub fn register_name(region: &str, offset: u64) -> String {
    let name = if region.starts_with("virtio") {
        match offset {
            0x00 => "MagicValue",
            0x04 => "Version",
            0x08 => "DeviceID",
            0x10 => "DeviceFeatures",
            0x14 => "DeviceFeaturesSel",
            0x20 => "DriverFeatures",
            0x24 => "DriverFeaturesSel",
            0x30 => "QueueSel",
            0x34 => "QueueNumMax",
            0x38 => "QueueNum",
            0x44 => "QueueReady",
            0x50 => "QueueNotify",
            0x60 => "InterruptStatus",
            0x64 => "InterruptACK",
            0x70 => "Status",
            0xfc => "ConfigGeneration",
            o if o >= 0x100 => return format!("config+{:#x}", o - 0x100),
            _ => "",
        }
    } else if region == "pl011" {
        match offset {
            0x00 => "DR",
            0x18 => "FR",
            0x30 => "CR",
            0x38 => "IMSC",
            0x3c => "RIS",
            0x40 => "MIS",
            0x44 => "ICR",
            _ => "",
        }
    } else if region == "goldfish-pipe" {
        match offset {
            0 => "CMD",
            8 => "SIGNAL_BUFFER",
            24 => "OPEN_BUFFER",
            36 => "VERSION",
            48 => "GET_SIGNALLED",
            _ => "",
        }
    } else if region == "goldfish-as" {
        match offset {
            0 => "COMMAND",
            4 => "STATUS",
            28 => "PING",
            52 => "PING_WITH_DATA",
            _ => "",
        }
    } else {
        ""
    };
    if name.is_empty() { format!("{offset:#x}") } else { name.into() }
}

/// Little-endian value of a 1–8 byte access.
pub fn le_value(data: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
    u64::from_le_bytes(b)
}

/// Stores `v` into a 1–8 byte access buffer, little-endian.
pub fn set_le(data: &mut [u8], v: u64) {
    let n = data.len().min(8);
    data[..n].copy_from_slice(&v.to_le_bytes()[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Probe(Mutex<Vec<(u64, u64)>>);
    impl MmioDevice for Probe {
        fn read(&self, offset: u64, data: &mut [u8]) {
            set_le(data, offset);
        }
        fn write(&self, offset: u64, data: &[u8]) {
            self.0.lock().expect("lock").push((offset, le_value(data)));
        }
    }

    #[test]
    fn dispatches_by_range() {
        let a = Arc::new(Probe::default());
        let mut bus = Bus::new();
        bus.insert(0x1000, 0x100, a.clone());
        bus.insert(0x3000, 0x100, Arc::new(Probe::default()));
        let mut buf = [0u8; 4];
        assert!(bus.read(0x1010, &mut buf));
        assert_eq!(le_value(&buf), 0x10);
        assert!(!bus.read(0x1100, &mut buf), "one past the end");
        assert!(!bus.read(0x0fff, &mut buf));
        assert!(bus.write(0x10fc, &7u32.to_le_bytes()));
        assert_eq!(a.0.lock().expect("lock").as_slice(), &[(0xfc, 7)]);
    }

    #[test]
    fn dispatch_reports_the_range_and_offset() {
        let mut bus = Bus::new();
        bus.insert(0x3000, 0x100, Arc::new(Probe::default()));
        bus.insert(0x1000, 0x100, Arc::new(Probe::default()));
        let mut buf = [0u8; 4];
        assert_eq!(bus.read_at(0x1010, &mut buf), Some((0, 0x10)), "indexed in base order");
        assert_eq!(bus.write_at(0x30fc, &buf), Some((1, 0xfc)));
        assert_eq!(bus.read_at(0x2000, &mut buf), None);
        assert_eq!(bus.regions().collect::<Vec<_>>(), [(0x1000, 0x100), (0x3000, 0x100)]);
    }

    /// Notes in `log` when it is dropped.
    struct DropNote(&'static str, Arc<Mutex<Vec<String>>>);
    impl MmioDevice for DropNote {
        fn read(&self, _: u64, _: &mut [u8]) {}
        fn write(&self, _: u64, _: &[u8]) {}
    }
    impl Drop for DropNote {
        fn drop(&mut self) {
            self.1.lock().expect("lock").push(format!("dropped {}", self.0));
        }
    }

    #[test]
    fn release_names_each_device_before_it_goes() {
        use crate::layout::*;
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bus = Bus::new();
        bus.insert(virtio_slot_addr(slots::SYSTEM), VIRTIO_MMIO_STRIDE, Arc::new(DropNote("blk", log.clone())));
        bus.insert(UART_BASE, 0x1000, Arc::new(DropNote("uart", log.clone())));
        bus.release_each(|name| log.lock().expect("lock").push(format!("releasing {name}")));
        let order = ["releasing pl011", "dropped uart", "releasing virtio-blk-system", "dropped blk"];
        // Base order, as the bus indexes them; the UART sits below the virtio slots.
        assert!(UART_BASE < virtio_slot_addr(0));
        assert_eq!(*log.lock().expect("lock"), order);
    }

    #[test]
    fn profile_env_values() {
        assert_eq!(profile_target(None), None);
        assert_eq!(profile_target(Some("")), None);
        assert_eq!(profile_target(Some("0")), None);
        assert_eq!(profile_target(Some("1")), Some(None));
        assert_eq!(profile_target(Some("/tmp/p")), Some(Some(PathBuf::from("/tmp/p"))));
    }

    #[test]
    fn regions_are_named_after_the_stock_board() {
        use crate::layout::*;
        assert_eq!(region_name(UART_BASE), "pl011");
        assert_eq!(region_name(virtio_slot_addr(slots::SYSTEM)), "virtio-blk-system");
        assert_eq!(region_name(virtio_slot_addr(3)), "virtio3");
        assert_eq!(region_name(0x1234_5000), "mmio@0x12345000");
        assert_eq!(register_name("virtio-gpu", 0x50), "QueueNotify");
        assert_eq!(register_name("virtio-gpu", 0x108), "config+0x8");
        assert_eq!(register_name("pl011", 0x18), "FR");
        assert_eq!(register_name("pl031", 0x0), "0x0");
    }

    fn profiled_bus() -> (Bus, ExitProfile) {
        use crate::layout::*;
        let mut bus = Bus::new();
        bus.insert(UART_BASE, PERIPH_SIZE, Arc::new(Probe::default()));
        bus.insert(PCI_ECAM_BASE, PCI_ECAM_SIZE, Arc::new(Probe::default()));
        let p = ExitProfile::new(&bus, 2);
        (bus, p)
    }

    #[test]
    fn profile_counts_exits_per_region_and_register() {
        use crate::layout::*;
        let (bus, p) = profiled_bus();
        let mut buf = [0u8; 4];
        for _ in 0..3 {
            p.record(0, bus.read_at(UART_BASE + 0x18, &mut buf), false, 100);
        }
        p.record(1, bus.write_at(UART_BASE, &buf), true, 50);
        p.record(1, bus.write_at(PCI_ECAM_BASE + 0x10_0000, &buf), true, 7);
        p.record(0, bus.read_at(0x10, &mut buf), false, 1);
        let dump = p.dump();
        let lines: Vec<&str> = dump.lines().collect();
        assert!(lines[0].starts_with("T "), "{dump}");
        assert_eq!(lines[1], "C 0 0 0 0 301");
        assert_eq!(lines[2], "C 1 0 0 0 57");
        assert!(lines.contains(&"R pl011 0x9000000 3 1 300 50"), "{dump}");
        assert!(lines.contains(&"O pl011 0x0 0 1"));
        assert!(lines.contains(&"O pl011 0x18 3 0"));
        assert!(lines.contains(&"O pci-ecam 0xffc 0 1"), "offsets past 4 KiB share the last bucket: {dump}");
        assert!(lines.contains(&"R unmapped 0x0 1 0 1 0"));
        let summary = p.summary();
        assert!(summary.contains("FR=3r/0w"), "{summary}");
        assert!(summary.find("pl011").expect("pl011") < summary.find("pci-ecam").expect("ecam"), "busiest first: {summary}");
    }

    #[test]
    fn first_entry_is_recorded_once() {
        let (_, p) = profiled_bus();
        assert!(p.summary().contains("first guest entry NaN"));
        p.entered();
        let first = |p: &ExitProfile| p.dump().split_whitespace().nth(2).expect("field").parse::<u64>().expect("number");
        let t = first(&p);
        std::thread::sleep(std::time::Duration::from_millis(2));
        p.entered();
        assert_eq!(first(&p), t);
    }

    /// vCPUs recording at once lose no counts.
    #[test]
    fn concurrent_records_all_land() {
        use crate::layout::*;
        let (bus, p) = profiled_bus();
        std::thread::scope(|s| {
            for cpu in 0..2 {
                let (bus, p) = (&bus, &p);
                s.spawn(move || {
                    let mut buf = [0u8; 4];
                    for _ in 0..10_000 {
                        p.record(cpu, bus.read_at(UART_BASE + 0x40, &mut buf), false, 1);
                    }
                });
            }
        });
        assert!(p.dump().contains("O pl011 0x40 20000 0"));
    }

    #[test]
    #[should_panic(expected = "overlaps")]
    fn rejects_overlap() {
        let mut bus = Bus::new();
        bus.insert(0x1000, 0x100, Arc::new(Probe::default()));
        bus.insert(0x10f0, 0x100, Arc::new(Probe::default()));
    }
}
