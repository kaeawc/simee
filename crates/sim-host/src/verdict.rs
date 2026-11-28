//! The admit / hold / backoff decision and the pressure rules behind it.
//!
//! Ported from spectra (MIT, same author): `internal/hostload/verdict.go` (`Decide`, the default
//! thresholds), `internal/pressure/assess.go` (the PSI thresholds) and `internal/memstate/linux.go`
//! (`linuxPressure`). One deliberate difference: spectra's macOS reading used a computed
//! "free percent", which kaeawc/spectra#754 found misjudges the machine, so on macOS memory is
//! judged by the kernel's own verdict (`kern.memorystatus_vm_pressure_level`), never by a byte
//! threshold: a Mac with a lot of compressible memory is not short of memory.

use serde::Serialize;

use crate::{Level, Psi, PsiResource, ThermalState};

/// The admission decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The host can take another job.
    Admit,
    /// Wait: busy or short of memory, but not in trouble.
    Hold,
    /// Shed work: memory is critically short.
    Backoff,
}

impl Verdict {
    /// `spectra load`'s exit status for the verdict.
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Admit => 0,
            Verdict::Hold => 3,
            Verdict::Backoff => 4,
        }
    }
}

/// The admission limits: spectra's defaults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Thresholds {
    /// Hold when the 1-minute load is at or above this times the CPU count.
    pub max_load_per_cpu: f64,
    /// Hold below this much available memory (GiB; where a byte count is known: Linux).
    pub min_free_gb: f64,
    /// Back off below this much available memory (GiB; where a byte count is known: Linux).
    pub backoff_free_gb: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { max_load_per_cpu: 1.5, min_free_gb: 8.0, backoff_free_gb: 4.0 }
    }
}

/// What the kernel says about memory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemReading {
    /// Available bytes, where the platform reports them (Linux `MemAvailable`; `None` on macOS,
    /// which judges by `level` alone).
    pub available_bytes: Option<u64>,
    /// The pressure level: macOS's kernel level, Linux's [`linux_pressure`].
    pub level: Level,
}

/// The power facts the verdict weighs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerReading {
    pub on_battery: bool,
    pub charge_percent: Option<u8>,
    /// macOS Low Power Mode, or Linux's `low-power` platform profile.
    pub low_power: bool,
}

/// On battery below this charge the host should shed work, not just wait.
pub const BATTERY_BACKOFF_PCT: u8 = 20;

/// One point-in-time host measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// The 1-minute load, `None` when it could not be read.
    pub load1: Option<f64>,
    pub cpus: usize,
    /// `None` when memory could not be read.
    pub mem: Option<MemReading>,
    /// `None` when the host has no thermal signal (not a failure: many hosts have none).
    pub thermal: Option<ThermalState>,
    /// `None` when power could not be read.
    pub power: Option<PowerReading>,
}

/// A verdict plus the single fact that decided it, and every fact that argued against admitting.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub verdict: Verdict,
    /// The signal that decided: the first trigger of the verdict's severity.
    pub reason: String,
    /// Every trigger (backoff, then hold); one admit sentence when there is none.
    pub reasons: Vec<String>,
    /// `max_load_per_cpu` times the CPU count, to two decimals.
    pub load_limit: f64,
}

/// Who reports the level: Linux (a byte count is known) derives it from PSI stalls, macOS is the
/// kernel's own memorystatus verdict.
fn pressure_source(m: &MemReading) -> &'static str {
    if m.available_bytes.is_some() { "PSI stalls report" } else { "the kernel reports" }
}

const GIB: f64 = (1u64 << 30) as f64;

/// Backoff first, then hold, then admit; a reading exactly at a threshold passes it. A missing
/// load or memory reading fails closed (hold): this gates starting more work. Thermal `serious`
/// holds and `critical` backs off; on battery holds (below [`BATTERY_BACKOFF_PCT`] it backs off),
/// and so does Low Power Mode. Each reason names its signal.
pub fn decide(r: &Reading, t: &Thresholds) -> Decision {
    let cpus = r.cpus.max(1);
    // The kernel reports load to 2 decimals; round so 1.1 x 3 is 3.30, not 3.3000000000000003.
    let limit = (t.max_load_per_cpu * cpus as f64 * 100.0).round() / 100.0;
    let avail_gb = r.mem.and_then(|m| m.available_bytes).map(|b| b as f64 / GIB);
    let mut back: Vec<String> = Vec::new();
    let mut hold: Vec<String> = Vec::new();
    if let Some(a) = avail_gb.filter(|a| *a < t.backoff_free_gb) {
        back.push(format!("memory: available memory {a:.1} GiB is below the backoff threshold {:.1} GiB", t.backoff_free_gb));
    }
    if let Some(m) = r.mem.filter(|m| m.level == Level::Critical) {
        back.push(format!("memory: {} critical memory pressure", pressure_source(&m)));
    }
    if r.thermal == Some(ThermalState::Critical) {
        back.push("thermal: the host is at a critical thermal state and throttling hard".to_owned());
    }
    if let Some(p) = r.power.filter(|p| p.on_battery)
        && let Some(c) = p.charge_percent.filter(|c| *c < BATTERY_BACKOFF_PCT)
    {
        back.push(format!("power: on battery at {c}%, below {BATTERY_BACKOFF_PCT}%: shed devices or plug in"));
    }
    match r.load1 {
        Some(l) if l >= limit => {
            hold.push(format!("load: 1-minute load {l:.2} is at or above the limit {limit:.2} ({} x {cpus} CPUs)", t.max_load_per_cpu))
        }
        None => hold.push("load: load could not be read".to_owned()),
        _ => {}
    }
    match (r.mem, avail_gb) {
        (None, _) => hold.push("memory: memory could not be read".to_owned()),
        (Some(_), Some(a)) if a < t.min_free_gb && a >= t.backoff_free_gb => {
            hold.push(format!("memory: available memory {a:.1} GiB is below the minimum {:.1} GiB", t.min_free_gb))
        }
        (Some(m), _) if m.level == Level::Warning => hold.push(format!("memory: {} memory pressure (warning)", pressure_source(&m))),
        _ => {}
    }
    if r.thermal == Some(ThermalState::Serious) {
        hold.push("thermal: the host is at a serious thermal state: more devices would throttle everything".to_owned());
    }
    if let Some(p) = r.power {
        if p.on_battery {
            let at = p.charge_percent.map(|c| format!(" at {c}%")).unwrap_or_default();
            hold.push(format!("power: on battery{at}: booting more devices drains it; plug in or boot anyway"));
        }
        if p.low_power {
            hold.push("power: low power mode is on: the system throttles background work and clocks".to_owned());
        }
    }
    let (verdict, reasons) = if !back.is_empty() {
        back.extend(hold);
        (Verdict::Backoff, back)
    } else if !hold.is_empty() {
        (Verdict::Hold, hold)
    } else {
        let mem = match avail_gb {
            Some(a) => format!("available memory {a:.1} GiB is at least {:.1} GiB", t.min_free_gb),
            None => "memory pressure is normal".to_owned(),
        };
        let load = r.load1.unwrap_or_default();
        (Verdict::Admit, vec![format!("1-minute load {load:.2} is below the limit {limit:.2} and {mem}")])
    };
    Decision { verdict, reason: reasons[0].clone(), reasons, load_limit: limit }
}

// Linux pressure: PSI stalls (spectra internal/memstate/linux.go's PSI rules).
const PSI_SOME_WARN_PCT: f64 = 10.0;
const PSI_FULL_WARN_PCT: f64 = 1.0;
const PSI_FULL_CRIT_PCT: f64 = 10.0;

/// Linux has no kernel pressure level like macOS's memorystatus, so the level comes from how long
/// tasks stalled waiting for memory (PSI). `MemAvailable` is reported (`avail_pct`) but never
/// drives the level: a low percentage on a big host with no stalls is not pressure (spectra's
/// verdict does not hold on it either). Absolute free GiB is judged separately, by the verdict's
/// `min_free_gb`/`backoff_free_gb` (spectra's defaults, kept). Without PSI the level is normal.
pub fn linux_pressure(avail_pct: f64, mem: Option<&PsiResource>) -> (Level, String) {
    let full10 = mem.and_then(|m| m.full).map(|f| f.avg10);
    let some10 = mem.map(|m| m.some.avg10);
    if let Some(f) = full10.filter(|f| *f >= PSI_FULL_CRIT_PCT) {
        return (Level::Critical, format!("all tasks stalled on memory {f:.1}% of the last 10s"));
    }
    if let Some(f) = full10.filter(|f| *f >= PSI_FULL_WARN_PCT) {
        return (Level::Warning, format!("all tasks stalled on memory {f:.1}% of the last 10s"));
    }
    if let Some(s) = some10.filter(|s| *s >= PSI_SOME_WARN_PCT) {
        return (Level::Warning, format!("some tasks stalled on memory {s:.1}% of the last 10s"));
    }
    (Level::Normal, format!("{avail_pct:.1}% of memory available, no stalls"))
}

/// A conclusion drawn from PSI (spectra `internal/pressure/assess.go`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub kind: String,
    pub severity: Level,
    pub summary: String,
}

const SOME_WARN_PCT: f64 = 10.0;
const SOME_CRIT_PCT: f64 = 30.0;
const FULL_WARN_PCT: f64 = 1.0;
const FULL_CRIT_PCT: f64 = 10.0;

fn by_severity(v: f64, warn: f64, crit: f64) -> Option<Level> {
    if v >= crit {
        Some(Level::Critical)
    } else if v >= warn {
        Some(Level::Warning)
    } else {
        None
    }
}

/// Findings from the host's PSI; a healthy host has none. Percentages of wall time over the
/// trailing 10 s: a `some` stall above ~10% is noticeable latency, and any sustained `full` stall
/// (memory and I/O only: CPU's is not meaningful) means the resource is the bottleneck.
pub fn assess_psi(p: &Psi) -> Vec<Finding> {
    let mut out = Vec::new();
    for (name, r) in [("cpu", &p.cpu), ("memory", &p.memory), ("io", &p.io)] {
        if let Some(sev) = by_severity(r.some.avg10, SOME_WARN_PCT, SOME_CRIT_PCT) {
            out.push(Finding {
                kind: format!("{name}-pressure"),
                severity: sev,
                summary: format!(
                    "host {name}: tasks stalled on {name} {:.1}% of the last 10s (60s avg {:.1}%)",
                    r.some.avg10, r.some.avg60
                ),
            });
        }
        if let Some(f) = r.full.filter(|_| name != "cpu")
            && let Some(sev) = by_severity(f.avg10, FULL_WARN_PCT, FULL_CRIT_PCT)
        {
            out.push(Finding {
                kind: format!("{name}-stall"),
                severity: sev,
                summary: format!(
                    "host {name}: ALL non-idle tasks stalled on {name} {:.1}% of the last 10s, so it is the bottleneck",
                    f.avg10
                ),
            });
        }
    }
    out
}

/// macOS `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical. Anything else is
/// not a level the kernel defines.
pub fn macos_level(raw: u32) -> Option<Level> {
    match raw {
        1 => Some(Level::Normal),
        2 => Some(Level::Warning),
        4 => Some(Level::Critical),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PsiLine;

    const GB: u64 = 1 << 30;

    fn mem(avail_gb: Option<u64>, level: Level) -> Option<MemReading> {
        Some(MemReading { available_bytes: avail_gb.map(|g| g * GB), level })
    }

    fn d(load1: Option<f64>, cpus: usize, mem: Option<MemReading>) -> Decision {
        decide(&Reading { load1, cpus, mem, thermal: None, power: None }, &Thresholds::default())
    }

    #[test]
    fn defaults_are_spectras() {
        assert_eq!(Thresholds::default(), Thresholds { max_load_per_cpu: 1.5, min_free_gb: 8.0, backoff_free_gb: 4.0 });
    }

    #[test]
    fn the_verdict_table() {
        // (load1, cpus, mem, verdict, reason fragment)
        let cases = [
            (Some(1.0), 4, mem(Some(16), Level::Normal), Verdict::Admit, "below the limit 6.00 and available memory 16.0 GiB"),
            (Some(1.0), 4, mem(None, Level::Normal), Verdict::Admit, "memory pressure is normal"),
            (Some(1.0), 4, mem(Some(3), Level::Normal), Verdict::Backoff, "below the backoff threshold 4.0 GiB"),
            (Some(99.0), 4, mem(Some(3), Level::Normal), Verdict::Backoff, "backoff"), // backoff outranks hold
            (Some(1.0), 4, mem(None, Level::Critical), Verdict::Backoff, "the kernel reports critical memory pressure"),
            (Some(1.0), 4, mem(Some(16), Level::Critical), Verdict::Backoff, "PSI stalls report critical memory pressure"),
            (Some(6.0), 4, mem(Some(16), Level::Normal), Verdict::Hold, "at or above the limit 6.00 (1.5 x 4 CPUs)"),
            (Some(5.99), 4, mem(Some(16), Level::Normal), Verdict::Admit, "below the limit"),
            (Some(1.0), 4, mem(Some(6), Level::Normal), Verdict::Hold, "below the minimum 8.0 GiB"),
            (Some(1.0), 4, mem(Some(8), Level::Normal), Verdict::Admit, "at least 8.0 GiB"), // exactly at a threshold passes
            (Some(1.0), 4, mem(Some(4), Level::Normal), Verdict::Hold, "below the minimum"), // 4 GiB is not below backoff
            (Some(1.0), 4, mem(None, Level::Warning), Verdict::Hold, "the kernel reports memory pressure (warning)"),
            (Some(1.0), 4, mem(Some(16), Level::Warning), Verdict::Hold, "PSI stalls report memory pressure (warning)"),
            (Some(1.0), 4, None, Verdict::Hold, "memory could not be read"),
            (None, 4, mem(Some(16), Level::Normal), Verdict::Hold, "load could not be read"),
            (Some(0.0), 0, mem(Some(16), Level::Normal), Verdict::Admit, "limit 1.50"), // zero CPUs count as one
        ];
        for (load, cpus, m, want, frag) in cases {
            let got = d(load, cpus, m);
            assert_eq!(got.verdict, want, "{load:?} {cpus} {m:?}: {}", got.reason);
            assert!(got.reason.contains(frag), "{:?} should contain {frag:?}", got.reason);
        }
    }

    fn full(thermal: Option<ThermalState>, power: Option<PowerReading>) -> Decision {
        let r = Reading { load1: Some(1.0), cpus: 4, mem: mem(None, Level::Normal), thermal, power };
        decide(&r, &Thresholds::default())
    }

    fn pw(on_battery: bool, pct: Option<u8>, low_power: bool) -> Option<PowerReading> {
        Some(PowerReading { on_battery, charge_percent: pct, low_power })
    }

    #[test]
    fn thermal_branches() {
        use ThermalState::*;
        assert_eq!(full(None, None).verdict, Verdict::Admit, "no signal is not a failure");
        assert_eq!(full(Some(Nominal), None).verdict, Verdict::Admit);
        assert_eq!(full(Some(Fair), None).verdict, Verdict::Admit);
        let s = full(Some(Serious), None);
        assert_eq!(s.verdict, Verdict::Hold);
        assert!(s.reason.starts_with("thermal: "), "{}", s.reason);
        let c = full(Some(Critical), None);
        assert_eq!(c.verdict, Verdict::Backoff);
        assert!(c.reason.starts_with("thermal: ") && c.reason.contains("critical"), "{}", c.reason);
    }

    #[test]
    fn power_branches() {
        assert_eq!(full(None, pw(false, Some(90), false)).verdict, Verdict::Admit, "AC");
        assert_eq!(full(None, pw(false, None, false)).verdict, Verdict::Admit, "AC, no battery");
        let b = full(None, pw(true, Some(34), false));
        assert_eq!(b.verdict, Verdict::Hold);
        assert_eq!(b.reason, "power: on battery at 34%: booting more devices drains it; plug in or boot anyway");
        assert_eq!(full(None, pw(true, None, false)).verdict, Verdict::Hold, "unknown charge still holds");
        assert_eq!(full(None, pw(true, Some(20), false)).verdict, Verdict::Hold, "20% is not below 20%");
        let low = full(None, pw(true, Some(19), false));
        assert_eq!(low.verdict, Verdict::Backoff);
        assert!(low.reason.starts_with("power: on battery at 19%"), "{}", low.reason);
        let lpm = full(None, pw(false, Some(90), true));
        assert_eq!(lpm.verdict, Verdict::Hold);
        assert!(lpm.reason.starts_with("power: low power mode"), "{}", lpm.reason);
        // Everything that applies is listed, the decisive one first.
        let both = full(Some(ThermalState::Serious), pw(true, Some(50), true));
        assert_eq!((both.verdict, both.reasons.len()), (Verdict::Hold, 3));
        let worst = full(Some(ThermalState::Critical), pw(true, Some(10), false));
        assert_eq!((worst.verdict, worst.reasons.len()), (Verdict::Backoff, 3));
    }

    #[test]
    fn the_load_limit_is_rounded_to_two_decimals() {
        let t = Thresholds { max_load_per_cpu: 1.1, ..Thresholds::default() };
        let got = decide(&Reading { load1: Some(3.3), cpus: 3, mem: mem(None, Level::Normal), thermal: None, power: None }, &t);
        assert_eq!((got.load_limit, got.verdict), (3.3, Verdict::Hold), "1.1 x 3 is 3.30 and 3.30 is at the limit");
    }

    #[test]
    fn exit_codes_follow_spectra_load() {
        assert_eq!([Verdict::Admit, Verdict::Hold, Verdict::Backoff].map(Verdict::exit_code), [0, 3, 4]);
    }

    fn psi(some10: f64, full10: Option<f64>) -> PsiResource {
        let line = |avg10| PsiLine { avg10, avg60: 0.0, avg300: 0.0, total_us: 0 };
        PsiResource { some: line(some10), full: full10.map(line) }
    }

    #[test]
    fn linux_pressure_is_driven_by_psi_stalls() {
        let lvl = |avail, m: Option<PsiResource>| linux_pressure(avail, m.as_ref()).0;
        assert_eq!(lvl(50.0, None), Level::Normal);
        assert_eq!(lvl(50.0, Some(psi(0.0, Some(0.0)))), Level::Normal);
        // MemAvailable alone never drives the level, however low.
        assert_eq!(lvl(14.9, None), Level::Normal);
        assert_eq!(lvl(4.9, None), Level::Normal);
        assert_eq!(lvl(1.0, Some(psi(0.0, Some(0.0)))), Level::Normal);
        assert_eq!(lvl(50.0, Some(psi(10.0, None))), Level::Warning);
        assert_eq!(lvl(50.0, Some(psi(9.9, Some(0.99)))), Level::Normal);
        assert_eq!(lvl(50.0, Some(psi(0.0, Some(1.0)))), Level::Warning);
        assert_eq!(lvl(50.0, Some(psi(0.0, Some(10.0)))), Level::Critical);
        // The PSI full stall outranks plenty of memory; low memory outranks a mild stall.
        assert_eq!(lvl(90.0, Some(psi(0.0, Some(12.0)))), Level::Critical);
        assert_eq!(lvl(4.0, Some(psi(0.0, Some(1.5)))), Level::Warning);
    }

    #[test]
    fn assess_psi_findings() {
        let healthy = Psi { cpu: psi(0.0, None), memory: psi(0.0, Some(0.0)), io: psi(0.0, Some(0.0)) };
        assert!(assess_psi(&healthy).is_empty());
        let busy = Psi { cpu: psi(35.0, Some(50.0)), memory: psi(12.0, Some(1.5)), io: psi(0.0, Some(11.0)) };
        let f = assess_psi(&busy);
        let got: Vec<(&str, Level)> = f.iter().map(|x| (x.kind.as_str(), x.severity)).collect();
        assert_eq!(
            got,
            [
                ("cpu-pressure", Level::Critical), // cpu's full line is ignored
                ("memory-pressure", Level::Warning),
                ("memory-stall", Level::Warning),
                ("io-stall", Level::Critical),
            ]
        );
    }

    #[test]
    fn macos_levels() {
        assert_eq!([1, 2, 4].map(macos_level), [Some(Level::Normal), Some(Level::Warning), Some(Level::Critical)]);
        assert_eq!((macos_level(0), macos_level(3)), (None, None));
    }
}
