//! Host health for a simulator farm: load, memory pressure, swap, disk, thermal state and power,
//! sampled without forking, kept in a 15-minute ring, and judged by an admit / hold / backoff
//! verdict. The thresholds and rules are ported from spectra (MIT, same author); see `verdict`.
//! The same sampler attributes the host's load to each running device (`usage`), and `admission`
//! predicts what new devices would add to it.
//!
//! macOS reads sysctls, `NSProcessInfo` and IOKit through `sim-coresim` (this crate forbids
//! `unsafe`); Linux reads `/proc` and `/sys`; `statvfs` comes from rustix. Each reading is
//! independent: one that is unavailable is `None` in the snapshot, never an error. Windows reads
//! nothing yet: every reading is `None` there.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub mod admission;
pub mod parse;
pub mod ring;
pub mod sysfs;
pub mod usage;
pub mod verdict;

pub use ring::{Average, Point, Ring, ThermalTime, Trend};
pub use verdict::{Decision, Finding, MemReading, PowerReading, Reading, Thresholds, Verdict, decide};

/// Why a reading could not be taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The kernel doesn't offer it (PSI compiled out or off, no sysfs).
    Unavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unavailable on this host")
    }
}

impl std::error::Error for Error {}

/// Load averages.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Load {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// A pressure or severity level, ordered by severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Normal,
    Warning,
    Critical,
}

/// The system thermal state, as macOS names it (`NSProcessInfo.ThermalState`); Linux's is derived
/// from trip points and frequency caps by [`sysfs::linux_thermal_state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThermalState {
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl ThermalState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThermalState::Nominal => "nominal",
            ThermalState::Fair => "fair",
            ThermalState::Serious => "serious",
            ThermalState::Critical => "critical",
        }
    }
}

/// One pressure-stall line: the percent of the last 10/60/300 s tasks were stalled.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PsiLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_us: u64,
}

/// `/proc/pressure/<resource>`: `some` (at least one task stalled) and `full` (all non-idle tasks).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PsiResource {
    pub some: PsiLine,
    /// Absent for CPU on kernels before 5.13.
    pub full: Option<PsiLine>,
}

/// Linux pressure stall information.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Psi {
    pub cpu: PsiResource,
    pub memory: PsiResource,
    pub io: PsiResource,
}

/// Reads `<dir>/{cpu,memory,io}` (`/proc/pressure`). [`Error::Unavailable`] when PSI is off or
/// compiled out (the files are missing, or unreadable with `psi=0`).
pub fn read_psi(dir: &Path) -> Result<Psi, Error> {
    let one = |name: &str| std::fs::read_to_string(dir.join(name)).ok().and_then(|t| parse::psi_resource(&t)).ok_or(Error::Unavailable);
    Ok(Psi { cpu: one("cpu")?, memory: one("memory")?, io: one("io")? })
}

/// Memory, swap and pressure.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Memory {
    pub total_bytes: u64,
    /// Linux `MemAvailable`; `None` on macOS, which doesn't report a figure judged to be honest.
    pub available_bytes: Option<u64>,
    /// Percent free: Linux available over total; macOS `kern.memorystatus_level`, the kernel's own.
    pub free_percent: Option<f64>,
    pub level: Level,
    pub level_reason: String,
    /// macOS `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical.
    pub pressure_level: Option<u32>,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    /// Compressed memory held in RAM: Linux zswap. macOS has no sysctl for it (it would need
    /// `host_statistics64`), so `None`.
    pub compressor_bytes: Option<u64>,
    /// Linux pressure stall information; `None` when PSI is off.
    pub psi: Option<Psi>,
}

/// Free space on the data volume.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Disk {
    pub path: String,
    pub total_bytes: u64,
    /// Available to an unprivileged user.
    pub free_bytes: u64,
}

/// A thermal trip point of a zone (`passive`, `hot` or `critical`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Trip {
    pub kind: String,
    pub temp_c: f64,
    /// Degrees between the zone's temperature and the trip.
    pub margin_c: f64,
}

/// A Linux thermal zone.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ThermalZone {
    pub name: String,
    pub temp_c: f64,
    pub trips: Vec<Trip>,
}

/// Thermal state and throttling. `None` fields are unavailable on this host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Thermal {
    /// Whether any thermal input could be read.
    pub available: bool,
    pub state: Option<ThermalState>,
    pub reason: Option<String>,
    pub zones: Vec<ThermalZone>,
    /// Linux: mean `scaling_cur_freq` over `cpuinfo_max_freq` (informational: idle clocks are low).
    pub freq_ratio: Option<f64>,
    /// Linux: mean `scaling_max_freq` over `cpuinfo_max_freq`: under 1 the clock is capped.
    pub freq_cap_ratio: Option<f64>,
    /// Linux, Intel: cumulative core throttle events; the ring reports the deltas.
    pub throttle_count: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerSource {
    Ac,
    Battery,
    Unknown,
}

/// Power source, battery and the system's power preferences. `None` fields are unavailable.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Power {
    pub source: PowerSource,
    pub has_battery: bool,
    pub charge_percent: Option<u8>,
    pub charging: Option<bool>,
    pub minutes_remaining: Option<u32>,
    /// macOS Low Power Mode; Linux platform profile `low-power`.
    pub low_power: Option<bool>,
    /// Linux `/sys/firmware/acpi/platform_profile`.
    pub profile: Option<String>,
    /// Linux cpufreq governor (cpu0).
    pub governor: Option<String>,
    /// Linux cpufreq `energy_performance_preference` (cpu0).
    pub energy_preference: Option<String>,
    /// macOS energy mode (automatic/low/high): `pmset` is the only reader, and this crate never
    /// forks, so it is unavailable.
    pub energy_mode: Option<String>,
    pub summary: String,
}

/// One reading of the host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub at_unix_ms: u64,
    pub cpus: usize,
    pub load: Option<Load>,
    pub memory: Option<Memory>,
    pub disk: Option<Disk>,
    pub thermal: Thermal,
    pub power: Power,
    /// PSI findings (Linux); empty on a healthy host.
    pub findings: Vec<Finding>,
}

impl Snapshot {
    /// What the verdict weighs. `recent_throttle` is whether the kernel counted throttle events in
    /// the last minute (from the ring), which makes the thermal state at least `serious`.
    pub fn reading(&self, recent_throttle: bool) -> Reading {
        Reading {
            load1: self.load.map(|l| l.one),
            cpus: self.cpus,
            mem: self.memory.as_ref().map(|m| MemReading { available_bytes: m.available_bytes, level: m.level }),
            thermal: self.effective_thermal(recent_throttle).map(|t| t.0),
            power: Some(PowerReading {
                on_battery: self.power.source == PowerSource::Battery,
                charge_percent: self.power.charge_percent,
                low_power: self.power.low_power == Some(true),
            }),
        }
    }

    /// The thermal state with the ring's throttle evidence applied, and why.
    pub fn effective_thermal(&self, recent_throttle: bool) -> Option<(ThermalState, String)> {
        let base = self.thermal.state.zip(self.thermal.reason.clone());
        if recent_throttle && base.as_ref().is_none_or(|b| b.0 < ThermalState::Serious) {
            return Some((ThermalState::Serious, "the kernel counted CPU throttle events in the last minute".to_owned()));
        }
        base
    }
}

/// What the machine is, for `simee doctor`: the chip, the model, the macOS release and whether
/// it can run Hypervisor.framework guests. `None` fields are unavailable on this host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Machine {
    /// `machdep.cpu.brand_string` (`Apple M3 Max`, `Intel(R) Core(TM) i9-9980HK ...`).
    pub cpu: Option<String>,
    pub chip: Option<sim_core::support::Chip>,
    /// `hw.model` (`Mac15,9`).
    pub model: Option<String>,
    /// `sysctl.proc_translated`: this process runs under Rosetta (an x86_64 build on Apple silicon).
    pub translated: Option<bool>,
    pub macos: Option<sim_core::support::Version>,
    pub macos_build: Option<String>,
    /// `kern.hv_support`.
    pub hv_support: Option<bool>,
}

/// Reads [`Machine`]: a few sysctls and `SystemVersion.plist`, no process spawned.
pub fn machine() -> Machine {
    let (macos, macos_build) = std::fs::read_to_string(sim_core::support::SYSTEM_VERSION_PLIST)
        .ok()
        .and_then(|t| sim_core::support::Version::from_system_version(&t))
        .map_or((None, None), |(v, b)| (Some(v), b));
    let m = platform::machine();
    Machine { macos, macos_build, ..m }
}

/// The data volume: macOS keeps user data on its own volume under the sealed system volume.
pub const DATA_VOLUME: &str = if cfg!(target_os = "macos") { "/System/Volumes/Data" } else { "/" };

/// `statvfs` is unix's; the volume's size and free space on Windows are not read yet.
#[cfg(not(unix))]
fn disk(_path: &str) -> Option<Disk> {
    None
}

#[cfg(unix)]
fn disk(path: &str) -> Option<Disk> {
    let s = rustix::fs::statvfs(path).ok()?;
    let unit = if s.f_frsize > 0 { s.f_frsize } else { s.f_bsize };
    Some(Disk { path: path.to_owned(), total_bytes: s.f_blocks.saturating_mul(unit), free_bytes: s.f_bavail.saturating_mul(unit) })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use sim_coresim::host::{low_power_mode, power_sources, sysctl, thermal_state};

    fn int<const N: usize>(name: &str) -> Option<[u8; N]> {
        sysctl(name)?.get(..N)?.try_into().ok()
    }

    pub fn load() -> Option<Load> {
        parse::loadavg_struct(&sysctl("vm.loadavg")?)
    }

    pub fn memory() -> Option<Memory> {
        let total = u64::from_ne_bytes(int::<8>("hw.memsize")?);
        let raw = u32::from_ne_bytes(int::<4>("kern.memorystatus_vm_pressure_level")?);
        let level = verdict::macos_level(raw)?;
        let pct = int::<4>("kern.memorystatus_level").map(|b| f64::from(u32::from_ne_bytes(b)));
        let (swap_total, swap_used) = sysctl("vm.swapusage").and_then(|b| parse::swapusage_struct(&b)).unwrap_or((0, 0));
        let word = match level {
            Level::Normal => "normal",
            Level::Warning => "warn",
            Level::Critical => "critical",
        };
        Some(Memory {
            total_bytes: total,
            available_bytes: None,
            free_percent: pct,
            level,
            level_reason: match pct {
                Some(p) => format!("the kernel's memory pressure level is {word} ({p:.0}% free)"),
                None => format!("the kernel's memory pressure level is {word}"),
            },
            pressure_level: Some(raw),
            swap_total_bytes: swap_total,
            swap_used_bytes: swap_used,
            compressor_bytes: None,
            psi: None,
        })
    }

    pub fn thermal() -> Thermal {
        sysfs::mac_thermal(thermal_state())
    }

    fn text(name: &str) -> Option<String> {
        let b = sysctl(name)?;
        let s = String::from_utf8_lossy(&b).trim_end_matches('\0').trim().to_owned();
        (!s.is_empty()).then_some(s)
    }

    fn flag(name: &str) -> Option<bool> {
        int::<4>(name).map(|b| u32::from_ne_bytes(b) != 0)
    }

    pub fn machine() -> Machine {
        let cpu = text("machdep.cpu.brand_string");
        // `hw.optional.arm64` is 1 on Apple silicon even for a translated (x86_64) process, whose
        // brand string reads as the emulated Intel CPU's.
        let chip = match flag("hw.optional.arm64") {
            Some(true) => Some(sim_core::support::Chip::AppleSilicon),
            _ if cpu.as_deref().is_some_and(|c| c.contains("Intel")) => Some(sim_core::support::Chip::Intel),
            _ => None,
        };
        Machine {
            cpu,
            chip,
            model: text("hw.model"),
            translated: Some(flag("sysctl.proc_translated").unwrap_or(false)),
            macos: None,
            macos_build: None,
            hv_support: flag("kern.hv_support"),
        }
    }

    pub fn power() -> Power {
        let p = power_sources();
        sysfs::mac_power(p.providing.as_deref(), p.has_battery, p.percent, p.charging, p.minutes_remaining, low_power_mode())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn load() -> Option<Load> {
        parse::loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
    }

    pub fn memory() -> Option<Memory> {
        let m = parse::meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)?;
        let psi = read_psi(Path::new("/proc/pressure")).ok();
        let pct = m.available as f64 / m.total as f64 * 100.0;
        let (level, reason) = verdict::linux_pressure(pct, psi.as_ref().map(|p| &p.memory));
        Some(Memory {
            total_bytes: m.total,
            available_bytes: Some(m.available),
            free_percent: Some(pct),
            level,
            level_reason: reason,
            pressure_level: None,
            swap_total_bytes: m.swap_total,
            swap_used_bytes: m.swap_total.saturating_sub(m.swap_free),
            compressor_bytes: (m.zswap > 0).then_some(m.zswap),
            psi,
        })
    }

    pub fn thermal() -> Thermal {
        sysfs::thermal(Path::new("/sys"))
    }

    pub fn power() -> Power {
        sysfs::power(Path::new("/sys"))
    }

    /// Not a Mac: no chip in the matrix, no Hypervisor.framework.
    pub fn machine() -> Machine {
        Machine { cpu: None, chip: None, model: None, translated: None, macos: None, macos_build: None, hv_support: None }
    }
}

/// Takes one snapshot: a handful of sysctls or procfs reads, no process spawned.
pub fn sample() -> Snapshot {
    let memory = platform::memory();
    let findings = memory.as_ref().and_then(|m| m.psi).map(|p| verdict::assess_psi(&p)).unwrap_or_default();
    Snapshot {
        at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        load: platform::load(),
        memory,
        disk: disk(DATA_VOLUME).or_else(|| disk("/")),
        thermal: platform::thermal(),
        power: platform::power(),
        findings,
    }
}

/// The snapshot, the ring's averages, and the verdict.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub snapshot: Snapshot,
    /// 1, 5 and 15 minutes.
    pub averages: Vec<Average>,
    pub load_trend: Trend,
    /// The thermal state with the ring's throttle evidence applied.
    pub thermal_state: Option<ThermalState>,
    pub thresholds: Thresholds,
    pub verdict: Decision,
    /// Every reason the verdict is not a plain admit (the same list as `verdict.reasons`).
    pub reasons: Vec<String>,
    /// Set while the ring covers less than 15 minutes: the 15 minute average (and under 5 minutes
    /// the 5 minute one too) is partial, since the sampler runs only while read and a cold one has
    /// no history. The verdict is the newest sample's, with the kernel's own 1-minute load
    /// average; where the host counts throttle events, a resume takes two samples
    /// [`WAKE_GAP`] apart, so the throttle evidence covers that gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_note: Option<String>,
}

/// Builds a report from a snapshot and the ring behind it.
pub fn report(snapshot: Snapshot, ring: &Ring, thresholds: Thresholds) -> Report {
    let throttled = ring.throttle_events(60).is_some_and(|n| n > 0);
    let reading = snapshot.reading(throttled);
    let mut verdict = decide(&reading, &thresholds);
    // The thermal reason in the verdict is generic; say what the kernel saw.
    if let Some((_, why)) = snapshot.effective_thermal(throttled)
        && reading.thermal.is_some_and(|t| t >= ThermalState::Serious)
    {
        for r in verdict.reasons.iter_mut().filter(|r| r.starts_with("thermal: ")) {
            r.push_str(&format!(" ({why})"));
        }
        verdict.reason = verdict.reasons[0].clone();
    }
    let span = ring.span_s();
    let partial = if span < 300.0 { "the 5 and 15 minute averages are partial" } else { "the 15 minute average is partial" };
    let history_note = (span < 900.0 && ring.len() < ring::CAPACITY)
        .then(|| format!("history covers {span:.0} s ({} samples): {partial}; load1 is the kernel's own 1-minute average", ring.len()));
    Report {
        history_note,
        averages: ring.averages(),
        load_trend: ring.load_trend(snapshot.cpus),
        thermal_state: reading.thermal,
        thresholds,
        reasons: verdict.reasons.clone(),
        verdict,
        snapshot,
    }
}

fn point(t: f64, s: &Snapshot) -> Point {
    Point {
        t,
        load1: s.load.map(|l| l.one),
        free_percent: s.memory.as_ref().and_then(|m| m.free_percent),
        level: s.memory.as_ref().map_or(Level::Normal, |m| m.level),
        thermal: s.thermal.state,
        throttle_total: s.thermal.throttle_count,
        on_battery: (s.power.source != PowerSource::Unknown).then_some(s.power.source == PowerSource::Battery),
    }
}

struct Shared {
    state: Mutex<State>,
    /// Notified on stop and on resume from a park.
    stop: Condvar,
    /// Notified after each sample.
    tick: Condvar,
    /// The readings. Locked by whoever samples, never together with `state`.
    source: Mutex<Box<dyn Source>>,
    clock: Box<dyn Clock>,
}

struct State {
    ring: Ring,
    latest: Option<Snapshot>,
    usage: usage::Tracker,
    stopped: bool,
    /// Not sampling: nobody read within `linger`, or nobody has yet.
    parked: bool,
    /// Clock time of the sample the ring and tracker restarted at.
    began: Duration,
    last_read: Duration,
    linger: Duration,
}

impl State {
    fn parked(linger: Duration) -> State {
        State {
            ring: Ring::default(),
            latest: None,
            usage: usage::Tracker::new(std::thread::available_parallelism().map_or(1, |n| n.get())),
            stopped: false,
            parked: true,
            began: Duration::ZERO,
            last_read: Duration::ZERO,
            linger,
        }
    }

    /// A reader arrived at `now`. True when the sampler is parked and must take a sample first.
    fn touch(&mut self, now: Duration) -> bool {
        // A reader preempted between reading the clock and here must not shorten the window.
        self.last_read = self.last_read.max(now);
        self.parked
    }

    /// Whether the worker should sample at `now`; parks it instead once the last read is older
    /// than the linger window.
    fn due(&mut self, now: Duration) -> bool {
        if self.parked {
            return false;
        }
        if now.saturating_sub(self.last_read) > self.linger {
            self.parked = true;
            return false;
        }
        true
    }

    fn record(&mut self, now: Duration, s: Snapshot, o: Option<usage::Observation>) {
        self.ring.push(point(now.saturating_sub(self.began).as_secs_f64(), &s));
        self.latest = Some(s);
        if let Some(o) = o {
            self.usage.update(o);
        }
    }

    /// Resumes at `now` from a park: the history before it is gone (the gap would read as a
    /// quiet host, and a rate across it would span minutes), so the ring and the usage tracker
    /// start over with this sample.
    fn restart(&mut self, now: Duration, first: Snapshot, o: Option<usage::Observation>) {
        self.ring = Ring::default();
        self.usage = usage::Tracker::new(first.cpus);
        self.began = now;
        self.parked = false;
        self.record(now, first, o);
    }
}

impl Shared {
    fn new(linger: Duration, source: impl Source + 'static, clock: impl Clock + 'static) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State::parked(linger)),
            stop: Condvar::new(),
            tick: Condvar::new(),
            source: Mutex::new(Box::new(source)),
            clock: Box::new(clock),
        })
    }

    /// Every read of the sampler starts here: a parked sampler takes one sample on the caller's
    /// thread (so the answer is current; two, [`WAKE_GAP`] apart, where the host counts throttle
    /// events) and wakes its worker; a running one just notes the read, which keeps it running
    /// for another linger window.
    fn read(&self) {
        let now = self.clock.now();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !st.touch(now) {
                return;
            }
        }
        // One resumer at a time: it keeps the source until it has restarted the sampler, so the
        // readers queued behind it find it running and take no sample of their own.
        let mut src = self.source.lock().unwrap_or_else(|e| e.into_inner());
        if !self.state.lock().unwrap_or_else(|e| e.into_inner()).parked {
            return;
        }
        let (s, o) = (src.sample(), src.observe());
        // Throttling shows only as a rise in the kernel's counter, and one sample has no rise:
        // without a second one, a throttled host would be told to admit on its first check.
        let second = s.thermal.throttle_count.is_some().then(|| {
            self.clock.sleep(WAKE_GAP);
            (self.clock.now(), src.sample())
        });
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.restart(now, s, o);
        if let Some((at, s)) = second {
            st.record(at, s, None);
        }
        drop(st);
        drop(src);
        self.stop.notify_all();
        self.tick.notify_all();
    }

    /// One worker step: samples when due, parks when the readers have gone.
    fn step(&self) {
        let now = self.clock.now();
        if !self.state.lock().unwrap_or_else(|e| e.into_inner()).due(now) {
            return;
        }
        let (s, o) = {
            let mut src = self.source.lock().unwrap_or_else(|e| e.into_inner());
            (src.sample(), src.observe())
        };
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.parked || st.stopped {
            return;
        }
        st.record(now, s, o);
        drop(st);
        self.tick.notify_all();
    }
}

/// Monotonic time, for the linger window: the system's, or a test's.
trait Clock: Send + Sync {
    /// Time since an arbitrary fixed point.
    fn now(&self) -> Duration;
    /// Waits `d` on this clock.
    fn sleep(&self, d: Duration);
}

struct Monotonic(Instant);

impl Clock for Monotonic {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// How long the sampler keeps running after the last read.
pub const LINGER: Duration = Duration::from_secs(60);

/// On a resume where the host counts throttle events (Linux, Intel), the gap between the two
/// samples a cold verdict differences: one sample alone has no delta.
pub const WAKE_GAP: Duration = Duration::from_millis(250);

/// A background sampler: one snapshot per interval into the ring, and one pass over the host's
/// processes into the per-device usage tracker, but only while someone reads it. It starts
/// parked (no sample, no thread work); the first read takes a sample on the reader's thread and
/// starts it; it parks again [`LINGER`] after the last read. Dropping it stops the thread.
pub struct Sampler {
    shared: Arc<Shared>,
    interval: Duration,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Where the sampler's readings come from: the live host in production, a fixture in tests.
trait Source: Send {
    /// One host snapshot.
    fn sample(&mut self) -> Snapshot;
    /// One pass over the host's processes (`None` where they can't be read).
    fn observe(&mut self) -> Option<usage::Observation>;
}

/// The live host: sysctls or procfs, and libproc.
#[derive(Default)]
struct Live(usage::Reader);

impl Source for Live {
    fn sample(&mut self) -> Snapshot {
        sample()
    }
    fn observe(&mut self) -> Option<usage::Observation> {
        self.0.observe()
    }
}

impl Sampler {
    /// A parked sampler that samples every `interval`, on a thread named `simee-host`, while
    /// read.
    pub fn start(interval: Duration) -> Sampler {
        Sampler::start_with(interval, LINGER, Live::default(), Monotonic(Instant::now()))
    }

    fn start_with(interval: Duration, linger: Duration, source: impl Source + 'static, clock: impl Clock + 'static) -> Sampler {
        let shared = Shared::new(linger, source, clock);
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("simee-host".into())
            .spawn(move || {
                loop {
                    let Ok(guard) = worker.state.lock() else { return };
                    let Ok(guard) = worker.stop.wait_while(guard, |s| s.parked && !s.stopped) else { return };
                    let Ok((guard, _)) = worker.stop.wait_timeout_while(guard, interval, |s| !s.stopped) else { return };
                    if guard.stopped {
                        return;
                    }
                    drop(guard);
                    worker.step();
                }
            })
            .ok();
        Sampler { shared, interval, handle }
    }

    /// Whether the sampler is sampling now (not parked).
    pub fn running(&self) -> bool {
        !self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).parked
    }

    /// Every running device's usage, simee's and the host's totals (`devices_usage`). Right after
    /// the sampler (re)started there is no interval to rate yet: this waits for the second sample
    /// (at most a few intervals); after that it never waits.
    pub fn usage(&self) -> usage::Report {
        self.shared.read();
        let st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let wait = if usage::AVAILABLE && self.handle.is_some() { self.interval * 3 } else { Duration::ZERO };
        let (st, _) =
            self.shared.tick.wait_timeout_while(st, wait, |s| s.usage.samples() < 2 && !s.stopped).unwrap_or_else(|e| e.into_inner());
        usage_of(&st)
    }

    /// [`Sampler::usage`] without the wait: right after the sampler (re)started, the rates are
    /// `None`. What admission reads, which never holds a boot up.
    pub fn usage_now(&self) -> usage::Report {
        self.shared.read();
        usage_of(&self.shared.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The newest snapshot and the report on it. On a parked sampler that is a fresh sample, and
    /// the report's `history_note` says the averages are partial.
    pub fn report(&self, thresholds: Thresholds) -> Report {
        self.shared.read();
        let st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        match st.latest.clone() {
            Some(s) => report(s, &st.ring, thresholds),
            None => report(sample(), &st.ring, thresholds),
        }
    }

    /// A copy of the ring: only what was sampled since the last (re)start.
    pub fn ring(&self) -> Ring {
        self.shared.read();
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).ring.clone()
    }
}

fn usage_of(st: &State) -> usage::Report {
    let mut r = st.usage.report();
    r.host.memory_total_bytes = st.latest.as_ref().and_then(|s| s.memory.as_ref()).map(|m| m.total_bytes);
    r
}

impl Drop for Sampler {
    fn drop(&mut self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.stopped = true;
        }
        self.shared.stop.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The process-wide sampler (1 s while read, parked otherwise): what the `host` API method reads.
pub fn global() -> &'static Sampler {
    static S: OnceLock<Sampler> = OnceLock::new();
    S.get_or_init(|| Sampler::start(Duration::from_secs(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psi_unavailable_when_files_are_missing_or_unreadable() {
        let dir = std::env::temp_dir().join(format!("simee-host-psi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(read_psi(&dir), Err(Error::Unavailable));
        let line = "some avg10=1.00 avg60=2.00 avg300=3.00 total=5\n";
        std::fs::write(dir.join("cpu"), line).expect("cpu");
        std::fs::write(dir.join("memory"), format!("{line}full avg10=0.50 avg60=0.00 avg300=0.00 total=1\n")).expect("mem");
        assert_eq!(read_psi(&dir), Err(Error::Unavailable), "io is missing");
        std::fs::write(dir.join("io"), "").expect("io");
        assert_eq!(read_psi(&dir), Err(Error::Unavailable), "an empty file (psi=0) is not PSI");
        std::fs::write(dir.join("io"), line).expect("io");
        let p = read_psi(&dir).expect("psi");
        assert_eq!((p.cpu.full, p.memory.full.map(|f| f.avg10), p.io.some.avg60), (None, Some(0.5), 2.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fixed healthy host: no test below depends on the machine it runs on.
    fn fixture() -> Snapshot {
        Snapshot {
            at_unix_ms: 0,
            cpus: 4,
            load: Some(Load { one: 0.5, five: 0.5, fifteen: 0.5 }),
            memory: Some(Memory {
                total_bytes: 8 << 30,
                available_bytes: Some(4 << 30),
                free_percent: Some(50.0),
                level: Level::Normal,
                level_reason: "plenty free".into(),
                pressure_level: Some(1),
                swap_total_bytes: 0,
                swap_used_bytes: 0,
                compressor_bytes: None,
                psi: None,
            }),
            disk: Some(Disk { path: "/".into(), total_bytes: 100 << 30, free_bytes: 50 << 30 }),
            thermal: Thermal {
                available: true,
                state: Some(ThermalState::Nominal),
                reason: None,
                zones: vec![],
                freq_ratio: None,
                freq_cap_ratio: None,
                throttle_count: None,
            },
            power: Power {
                source: PowerSource::Ac,
                has_battery: false,
                charge_percent: None,
                charging: None,
                minutes_remaining: None,
                low_power: None,
                profile: None,
                governor: None,
                energy_preference: None,
                energy_mode: None,
                summary: "ac".into(),
            },
            findings: vec![],
        }
    }

    /// A fake host: every read succeeds at once, and each observation is 10 ms later with the
    /// host's CPU half busy.
    #[derive(Default)]
    struct Fake {
        n: u32,
    }

    impl Source for Fake {
        fn sample(&mut self) -> Snapshot {
            fixture()
        }
        fn observe(&mut self) -> Option<usage::Observation> {
            self.n += 1;
            Some(usage::Observation {
                at_ns: u64::from(self.n) * 10_000_000,
                host_ticks: Some([self.n * 5, 0, self.n * 5, 0]),
                ..usage::Observation::default()
            })
        }
    }

    fn snap(thermal: Option<ThermalState>) -> Snapshot {
        let mut s = fixture();
        s.thermal =
            Thermal { available: thermal.is_some(), state: thermal, reason: thermal.map(|t| format!("it is {}", t.as_str())), ..s.thermal };
        s
    }

    #[test]
    fn recent_throttle_events_raise_thermal_to_serious() {
        let s = snap(Some(ThermalState::Nominal));
        assert_eq!(s.effective_thermal(false).map(|t| t.0), Some(ThermalState::Nominal));
        assert_eq!(s.effective_thermal(true).map(|t| t.0), Some(ThermalState::Serious));
        assert_eq!(snap(Some(ThermalState::Critical)).effective_thermal(true).map(|t| t.0), Some(ThermalState::Critical));
        assert_eq!(snap(None).effective_thermal(true).map(|t| t.0), Some(ThermalState::Serious), "counters without zones");
        assert_eq!(snap(None).effective_thermal(false), None);
    }

    #[test]
    fn report_names_the_thermal_evidence() {
        let mut ring = Ring::default();
        for (i, total) in [10u64, 12].into_iter().enumerate() {
            ring.push(Point { throttle_total: Some(total), ..point(i as f64, &snap(None)) });
        }
        let r = report(snap(Some(ThermalState::Nominal)), &ring, Thresholds::default());
        assert_eq!(r.thermal_state, Some(ThermalState::Serious));
        assert!(r.reasons.iter().any(|x| x.starts_with("thermal: ") && x.contains("throttle events")), "{:?}", r.reasons);
        assert_eq!(r.reasons, r.verdict.reasons);
    }

    #[test]
    fn the_sampler_fills_the_ring() {
        let s = Sampler::start_with(Duration::from_millis(10), LINGER, Fake::default(), Monotonic(Instant::now()));
        // The fake never blocks, so only the sampler thread's scheduling is in play: wait for 3
        // samples with a deadline generous enough for a heavily loaded host.
        let deadline = Instant::now() + Duration::from_secs(60);
        while s.ring().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ring = s.ring();
        assert!(ring.len() >= 3, "{}", ring.len());
        let r = s.report(Thresholds::default());
        assert!(r.averages[0].samples >= 3);
        let u = s.usage();
        assert_eq!(u.available, usage::AVAILABLE);
        assert!(u.samples >= 3 && u.host.cpu_pct.s1.is_some(), "{u:?}");
        assert_eq!(u.host.memory_total_bytes, Some(8 << 30));
        drop(s);
    }

    /// A clock a test moves by hand.
    #[derive(Clone, Default)]
    struct FakeClock(Arc<std::sync::atomic::AtomicU64>);

    impl FakeClock {
        fn advance(&self, d: Duration) {
            self.0.fetch_add(d.as_millis() as u64, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(std::sync::atomic::Ordering::SeqCst))
        }
        fn sleep(&self, d: Duration) {
            self.advance(d);
        }
    }

    /// A fake source that counts its reads and can report a hot host, or (Linux, Intel) a
    /// throttle counter that rises by `throttle` events per sample.
    #[derive(Clone, Default)]
    struct Counting {
        samples: Arc<std::sync::atomic::AtomicUsize>,
        observes: Arc<std::sync::atomic::AtomicUsize>,
        serious: bool,
        throttle: Option<u64>,
    }

    impl Counting {
        fn reads(&self) -> (usize, usize) {
            (self.samples.load(std::sync::atomic::Ordering::SeqCst), self.observes.load(std::sync::atomic::Ordering::SeqCst))
        }
    }

    impl Source for Counting {
        fn sample(&mut self) -> Snapshot {
            let n = self.samples.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u64 + 1;
            let mut s = snap(self.serious.then_some(ThermalState::Serious).or(Some(ThermalState::Nominal)));
            s.thermal.throttle_count = self.throttle.map(|k| 1000 + k * n);
            // A roomy host, so that only the thermal state can hold a boot.
            if let Some(m) = s.memory.as_mut() {
                (m.total_bytes, m.available_bytes, m.free_percent) = (128 << 30, Some(64 << 30), Some(50.0));
            }
            s
        }
        fn observe(&mut self) -> Option<usage::Observation> {
            self.observes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        }
    }

    const SECOND: Duration = Duration::from_secs(1);

    /// A sampler's core with no thread: the test plays the worker, one `step` per second.
    fn core(linger: Duration, serious: bool) -> (Arc<Shared>, FakeClock, Counting) {
        let (clock, src) = (FakeClock::default(), Counting { serious, ..Counting::default() });
        (Shared::new(linger, src.clone(), clock.clone()), clock, src)
    }

    fn parked(sh: &Shared) -> bool {
        sh.state.lock().expect("state").parked
    }

    #[test]
    fn nothing_is_sampled_before_the_first_read() {
        let (sh, clock, src) = core(LINGER, false);
        for _ in 0..30 {
            clock.advance(SECOND);
            sh.step();
        }
        assert_eq!(src.reads(), (0, 0));
        assert!(parked(&sh));
    }

    #[test]
    fn sampling_continues_while_reads_continue() {
        let (sh, clock, src) = core(LINGER, false);
        sh.read();
        assert_eq!(src.reads(), (1, 1), "the first read samples once, at once");
        // Far past one linger window in total, but never a window without a read.
        for _ in 0..10 {
            for _ in 0..50 {
                clock.advance(SECOND);
                sh.step();
            }
            sh.read();
        }
        assert!(!parked(&sh));
        assert_eq!(src.reads(), (1 + 500, 1 + 500), "a sample per step and no resume sample: reads kept it running");
        assert_eq!(sh.state.lock().expect("state").ring.len(), 501);
    }

    #[test]
    fn the_sampler_parks_after_the_linger_window_and_resumes_on_a_read() {
        let (sh, clock, src) = core(Duration::from_secs(60), false);
        sh.read();
        for _ in 0..60 {
            clock.advance(SECOND);
            sh.step();
        }
        assert!(!parked(&sh), "exactly the linger window after the read is still inside it");
        assert_eq!(src.reads().0, 61);
        clock.advance(SECOND);
        sh.step();
        assert!(parked(&sh));
        let after = src.reads();
        for _ in 0..600 {
            clock.advance(SECOND);
            sh.step();
        }
        assert_eq!(src.reads(), after, "parked: no reads of the host at all");
        // A read after the park starts over: one fresh sample, and the history before the gap is gone.
        sh.read();
        assert_eq!(src.reads().0, after.0 + 1);
        assert!(!parked(&sh));
        let st = sh.state.lock().expect("state");
        assert_eq!((st.ring.len(), st.ring.span_s()), (1, 0.0));
        assert_eq!(st.usage.samples(), 0, "the observation source gave none here, so the tracker restarted empty");
    }

    #[test]
    fn an_admission_check_on_a_cold_sampler_judges_a_fresh_sample_and_says_so() {
        use admission::{Item, Limits, admit, seed};
        let item = Item { platform: sim_core::platform::Platform::Ios, form_factor: None };
        for (serious, want) in [(false, Verdict::Admit), (true, Verdict::Hold)] {
            let (clock, src) = (FakeClock::default(), Counting { serious, ..Counting::default() });
            let s = Sampler { shared: Shared::new(LINGER, src.clone(), clock), interval: SECOND, handle: None };
            assert_eq!(src.reads(), (0, 0));
            let host = admission::HostState::from_report(&s.report(Thresholds::default()));
            assert_eq!(src.reads(), (1, 1), "one immediate sample");
            let a = admit(&host, &[item], 1, seed, &Limits::default());
            assert_eq!(a.verdict, want, "{:?}", a.reasons);
            let basis = a.host_basis.expect("a cold sampler says what the verdict used");
            assert!(
                basis.contains("1 samples") && basis.contains("5 and 15 minute averages are partial") && basis.contains("kernel"),
                "{basis}"
            );
        }
    }

    /// A sampler the test drives: no worker thread, a fake clock and a counting source.
    fn manual(src: &Counting) -> (Sampler, FakeClock) {
        let clock = FakeClock::default();
        (Sampler { shared: Shared::new(LINGER, src.clone(), clock.clone()), interval: SECOND, handle: None }, clock)
    }

    fn admit_one(s: &Sampler) -> admission::Admission {
        use admission::{Item, Limits, admit, seed};
        let item = Item { platform: sim_core::platform::Platform::Ios, form_factor: None };
        admit(&admission::HostState::from_report(&s.report(Thresholds::default())), &[item], 1, seed, &Limits::default())
    }

    /// Runs a read sampler until it parks, then lets it sit parked for ten minutes.
    fn park(s: &Sampler, clock: &FakeClock) {
        s.shared.read();
        while !parked(&s.shared) {
            clock.advance(SECOND);
            s.shared.step();
        }
        clock.advance(Duration::from_secs(600));
    }

    #[test]
    fn a_throttled_host_is_held_on_the_first_check_after_a_park() {
        // Linux, Intel: the zones are below their trips (nominal), but the throttle counter rises.
        let src = Counting { throttle: Some(3), ..Counting::default() };
        let (s, clock) = manual(&src);
        park(&s, &clock);
        let before = src.reads();
        let a = admit_one(&s);
        assert_eq!(a.verdict, Verdict::Hold, "{:?}", a.reasons);
        assert!(a.reasons.iter().any(|r| r.contains("throttle events")), "{:?}", a.reasons);
        assert_eq!(src.reads(), (before.0 + 2, before.1 + 1), "two samples a gap apart, one pass over the processes");
        let ring = s.ring();
        assert_eq!((ring.len(), ring.span_s()), (2, WAKE_GAP.as_secs_f64()), "the gap is on the sampler's clock");
    }

    #[test]
    fn a_cold_check_with_flat_throttle_counters_still_admits() {
        let src = Counting { throttle: Some(0), ..Counting::default() };
        let (s, clock) = manual(&src);
        park(&s, &clock);
        let a = admit_one(&s);
        assert_eq!(a.verdict, Verdict::Admit, "{:?}", a.reasons);
    }

    #[test]
    fn a_host_without_throttle_counters_resumes_on_one_sample() {
        // macOS reads its thermal state directly: no counter to difference, no wait.
        let src = Counting::default();
        let (s, clock) = manual(&src);
        park(&s, &clock);
        let before = src.reads();
        assert_eq!(admit_one(&s).verdict, Verdict::Admit);
        assert_eq!(src.reads(), (before.0 + 1, before.1 + 1));
        assert_eq!(s.ring().len(), 1);
    }

    /// Wraps [`Counting`]: the first pass over the processes (the last read a resume makes)
    /// says it has sampled, then waits for the test's go.
    struct Gated {
        inner: Counting,
        sampled: std::sync::mpsc::Sender<()>,
        go: Option<std::sync::mpsc::Receiver<()>>,
    }

    impl Source for Gated {
        fn sample(&mut self) -> Snapshot {
            self.inner.sample()
        }
        fn observe(&mut self) -> Option<usage::Observation> {
            let o = self.inner.observe();
            if let Some(go) = self.go.take() {
                let _ = self.sampled.send(());
                let _ = go.recv();
            }
            o
        }
    }

    #[test]
    fn a_resumer_keeps_the_source_until_it_has_restarted() {
        let (sampled, sampled_rx) = std::sync::mpsc::channel();
        let (go_tx, go) = std::sync::mpsc::channel();
        let counting = Counting::default();
        let sh = Shared::new(LINGER, Gated { inner: counting.clone(), sampled, go: Some(go) }, FakeClock::default());
        let first = {
            let sh = Arc::clone(&sh);
            std::thread::spawn(move || sh.read())
        };
        sampled_rx.recv().expect("the first reader sampled");
        // Hold the state, so the first reader cannot restart, and let it finish its reads.
        let st = sh.state.lock().expect("state");
        go_tx.send(()).expect("go");
        // If the source comes free while the sampler is still parked, a second reader waiting on
        // it would find the sampler parked and sample again.
        let mut free_while_parked = false;
        for _ in 0..100_000 {
            if sh.source.try_lock().is_ok() {
                free_while_parked = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(st.parked);
        drop(st);
        first.join().expect("first reader");
        assert!(!free_while_parked, "the resumer released the source before it restarted the sampler");
        // Readers after the resume find it running and take nothing.
        let readers: Vec<_> = (0..8)
            .map(|_| {
                let sh = Arc::clone(&sh);
                std::thread::spawn(move || sh.read())
            })
            .collect();
        for r in readers {
            r.join().expect("reader");
        }
        assert_eq!(counting.reads(), (1, 1), "exactly one sample for the resume");
    }

    #[test]
    fn the_last_read_never_moves_back() {
        let mut st = State::parked(LINGER);
        st.touch(Duration::from_secs(5));
        st.touch(Duration::from_secs(3));
        assert_eq!(st.last_read, Duration::from_secs(5), "a reader preempted after reading the clock");
    }

    #[test]
    fn the_note_stays_until_the_15_minute_average_is_whole() {
        let (sh, clock, _) = core(Duration::from_secs(3600), false);
        sh.read();
        let note = |sh: &Shared| {
            let st = sh.state.lock().expect("state");
            report(st.latest.clone().expect("latest"), &st.ring, Thresholds::default()).history_note
        };
        for _ in 0..301 {
            clock.advance(SECOND);
            sh.step();
        }
        let n = note(&sh).expect("5 minutes in, the 15 minute average is partial");
        assert!(n.contains("15 minute average is partial") && !n.contains("5 and 15"), "{n}");
        for _ in 301..ring::CAPACITY {
            clock.advance(SECOND);
            sh.step();
        }
        assert_eq!(note(&sh), None, "a full ring");
    }

    /// The live host through the real sampler: load-sensitive, so it only runs on request.
    #[test]
    #[ignore = "reads the live host; run with --ignored"]
    fn the_live_sampler_runs() {
        let s = Sampler::start(Duration::from_millis(10));
        let deadline = Instant::now() + Duration::from_secs(60);
        while s.ring().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s.ring().len() >= 3);
        assert!(s.usage().host.memory_total_bytes.is_some());
    }

    #[test]
    #[ignore = "reads the live host; run with --ignored"]
    fn the_live_host_reads_sane_values() {
        let s = sample();
        assert!(s.cpus >= 1);
        let load = s.load.expect("load average");
        assert!((0.0..10_000.0).contains(&load.one) && load.fifteen >= 0.0);
        let mem = s.memory.as_ref().expect("memory");
        assert!(mem.total_bytes > (1 << 28) && mem.free_percent.is_some_and(|p| (0.0..=100.0).contains(&p)));
        if cfg!(target_os = "macos") {
            assert!(matches!(mem.pressure_level, Some(1 | 2 | 4)));
        } else {
            assert!(mem.available_bytes.is_some_and(|a| a <= mem.total_bytes));
        }
        let disk = s.disk.as_ref().expect("disk");
        assert!(disk.total_bytes > 0 && disk.free_bytes <= disk.total_bytes, "{disk:?}");
        if cfg!(target_os = "macos") {
            assert!(s.thermal.state.is_some(), "macOS always reports a thermal state");
            assert_eq!(disk.path, "/System/Volumes/Data");
        }
        println!("thermal: {:?} ({:?}); power: {}; memory: {}", s.thermal.state, s.thermal.reason, s.power.summary, mem.level_reason);
        let r = report(s, &Ring::default(), Thresholds::default());
        assert!(!r.reasons.is_empty());
    }

    #[test]
    #[ignore = "reads the live host; run with --ignored"]
    fn the_machine_names_its_chip_and_release() {
        let m = machine();
        println!("{m:?}");
        if cfg!(target_os = "macos") {
            assert!(m.cpu.is_some() && m.model.is_some() && m.hv_support.is_some(), "{m:?}");
            assert!(m.macos.is_some_and(|v| v.major >= 11) && m.macos_build.is_some(), "{m:?}");
            let want = if cfg!(target_arch = "aarch64") || m.translated == Some(true) {
                sim_core::support::Chip::AppleSilicon
            } else {
                sim_core::support::Chip::Intel
            };
            assert_eq!(m.chip, Some(want));
        } else {
            assert_eq!((m.chip, m.hv_support), (None, None));
        }
    }

    /// `cargo test -p simee-host --release sample_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "measures the per-sample CPU cost"]
    fn sample_cost() {
        let n = 300;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(sample());
        }
        let per = t.elapsed() / n;
        // At 1 Hz, a sample's duration as a fraction of a second is its share of one core.
        println!("one sample: {per:?} = {:.4}% of a core at 1 Hz", per.as_secs_f64() * 100.0);
        assert!(per < Duration::from_millis(1), "{per:?}: over the 0.1% budget");
    }
}
