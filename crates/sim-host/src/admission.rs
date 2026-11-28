//! Admission: what new devices would cost this host, and whether to start them now (#275).
//!
//! A device's cost is predicted per platform and form factor: CPU and memory while it boots and
//! once it is idle. The defaults are docs/benchmarks.md's numbers ([`seed`]); an [`Estimator`]
//! refines the idle numbers from the devices this process has seen running (`devices_usage`'s
//! rows), a small rolling estimate kept in memory. [`admit`] adds the predicted cost to the host's
//! state and judges the result with the same admit / hold / backoff verdict as the `host` method
//! ([`crate::verdict`]), with simee's limits on memory used ([`Limits`]), and says why with the
//! numbers: "memory: 3 more Android phone devices would take memory used from 41% to 86%".
//!
//! [`pace`] starts work in chunks as the host admits it (`boot_many {admission: "wait"}`), on a
//! [`Clock`] a test can fake.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use sim_core::form_factor::FormFactor;
use sim_core::platform::Platform;

use crate::Report;
use crate::verdict::{Decision, Thresholds, Verdict};

const MB: u64 = 1_000_000;
const GIB: f64 = (1u64 << 30) as f64;

/// What one device costs the host: percent of one core and bytes, while it boots and once idle.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cost {
    pub idle_cpu_pct: f64,
    pub boot_cpu_pct: f64,
    pub idle_memory_bytes: u64,
    pub boot_memory_bytes: u64,
    /// Where the numbers come from.
    pub source: String,
}

/// The measured cost of a device, from docs/benchmarks.md (M3 Max, 2026-11), used until this host
/// has seen devices of the kind. Only phones are measured: every other form factor of a platform
/// takes its phone's numbers, and says so in `source`.
///
/// - Android (emu, API 36, 4 vCPU, 2 GB): idle 5.9% of a core ("Idle host CPU, the NAT"); idle
///   memory 2.8 GB ("Android farm": host memory used per device); booting memory 3764 MB (a cold
///   boot's peak RSS, "Android boot, API 36"); booting CPU 250% of a core (docs/farm-android.md:
///   each guest holds 2 to 3 of its 4 cores while it boots).
/// - iOS (iPhone 17, iOS 27.2, default profile): idle and booting memory 2176 MB ("iOS farm":
///   footprint per device); booting CPU 280% of a core (docs/farm.md: 131 CPU-s in the 46.8 s
///   until one clone settled); idle 10% of a core, the settle rule's limit (a settled device is
///   under it; idle CPU itself is not measured yet).
pub fn seed(platform: Platform, form_factor: FormFactor) -> Cost {
    let mut c = match platform {
        Platform::Android => Cost {
            idle_cpu_pct: 5.9,
            boot_cpu_pct: 250.0,
            idle_memory_bytes: 2800 * MB,
            boot_memory_bytes: 3764 * MB,
            source: "docs/benchmarks.md (an API 36 phone on emu)".into(),
        },
        Platform::Ios => Cost {
            idle_cpu_pct: 10.0,
            boot_cpu_pct: 280.0,
            idle_memory_bytes: 2176 * MB,
            boot_memory_bytes: 2176 * MB,
            source: "docs/benchmarks.md (an iPhone 17, default profile)".into(),
        },
    };
    if form_factor != FormFactor::Phone {
        c.source.push_str(&format!("; a {} isn't measured yet, so a phone's numbers", form_factor.as_str()));
    }
    c
}

/// A device under this much CPU (1-minute, percent of one core) counts as idle: its CPU and
/// footprint refine the idle estimate. A busier one may still be booting or running a test.
pub const IDLE_CPU_PCT: f64 = 50.0;
/// The weight of each new observation in the rolling estimate, which starts from the seed.
const WEIGHT: f64 = 0.2;
/// Observations of a kind before the estimate replaces the seed: one idle device, or the same
/// one polled twice, says little about the kind.
pub const MIN_SAMPLES: u64 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Seen {
    n: u64,
    idle_cpu_pct: f64,
    memory_bytes: f64,
}

/// A rolling estimate of what idle devices of each kind cost on this host: an average that starts
/// at the seed and moves a fifth of the way to each observation, used from [`MIN_SAMPLES`] on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Estimator {
    seen: HashMap<(Platform, FormFactor), Seen>,
}

impl Estimator {
    /// One running device: its 1-minute CPU (`None` before the sampler has a rate) and footprint.
    /// Only an idle device counts; one without a rate yet may be booting, so it doesn't either.
    pub fn observe(&mut self, platform: Platform, form_factor: FormFactor, cpu_1m: Option<f64>, footprint_bytes: u64) {
        let Some(cpu) = cpu_1m.filter(|c| *c < IDLE_CPU_PCT && footprint_bytes > 0) else { return };
        let s = self.seen.entry((platform, form_factor)).or_insert_with(|| {
            let seed = seed(platform, form_factor);
            Seen { n: 0, idle_cpu_pct: seed.idle_cpu_pct, memory_bytes: seed.idle_memory_bytes as f64 }
        });
        s.idle_cpu_pct += WEIGHT * (cpu - s.idle_cpu_pct);
        s.memory_bytes += WEIGHT * (footprint_bytes as f64 - s.memory_bytes);
        s.n += 1;
    }

    /// The cost of a device of this kind: the seed, with the idle numbers this host measured. The
    /// booting memory keeps the seed's ratio to idle memory; booting CPU stays the seed's.
    pub fn cost(&self, platform: Platform, form_factor: FormFactor) -> Cost {
        let s = seed(platform, form_factor);
        let Some(seen) = self.seen.get(&(platform, form_factor)).filter(|x| x.n >= MIN_SAMPLES) else { return s };
        let idle = seen.memory_bytes.round() as u64;
        let ratio = s.boot_memory_bytes as f64 / s.idle_memory_bytes.max(1) as f64;
        Cost {
            idle_cpu_pct: (seen.idle_cpu_pct * 10.0).round() / 10.0,
            boot_cpu_pct: s.boot_cpu_pct,
            idle_memory_bytes: idle,
            boot_memory_bytes: (idle as f64 * ratio).round() as u64,
            source: format!(
                "this host: {} samples of idle {} {} devices blended with the published numbers (idle CPU and memory; booting memory \
                 at the measured ratio); booting CPU from {}",
                seen.n,
                platform.label(),
                form_factor.as_str(),
                s.source
            ),
        }
    }
}

/// The process's estimate, refined by every admission it judges.
pub fn estimator() -> &'static Mutex<Estimator> {
    static E: OnceLock<Mutex<Estimator>> = OnceLock::new();
    E.get_or_init(Mutex::default)
}

/// simee's limits on top of the host verdict's: memory used after the new devices boot.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Limits {
    pub host: Thresholds,
    /// Hold when the new devices would take memory used to this percent or more.
    pub hold_memory_used_pct: f64,
    /// Back off at this percent or more.
    pub backoff_memory_used_pct: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { host: Thresholds::default(), hold_memory_used_pct: 85.0, backoff_memory_used_pct: 95.0 }
    }
}

/// What admission weighs of the host now.
#[derive(Clone, Debug, PartialEq)]
pub struct HostState {
    pub cpus: usize,
    pub load1: Option<f64>,
    pub memory_total_bytes: Option<u64>,
    /// Linux `MemAvailable`; `None` on macOS, which judges by the kernel's level and percent.
    pub memory_available_bytes: Option<u64>,
    /// Percent free: Linux available over total, macOS `kern.memorystatus_level`.
    pub memory_free_pct: Option<f64>,
    /// The host's own verdict now (thermal, power, memory pressure, load).
    pub now: Decision,
    /// What the verdict rests on when the sampler had little history (it runs only while read):
    /// the newest sample and the kernel's own load average. `None` with a full history.
    pub basis: Option<String>,
}

impl HostState {
    pub fn from_report(r: &Report) -> HostState {
        let m = r.snapshot.memory.as_ref();
        HostState {
            cpus: r.snapshot.cpus,
            load1: r.snapshot.load.map(|l| l.one),
            memory_total_bytes: m.map(|m| m.total_bytes),
            memory_available_bytes: m.and_then(|m| m.available_bytes),
            memory_free_pct: m.and_then(|m| m.free_percent),
            now: r.verdict.clone(),
            basis: r.history_note.clone(),
        }
    }
}

/// A new device to admit: its platform and form factor (`None`: unknown, judged as a phone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub platform: Platform,
    pub form_factor: Option<FormFactor>,
}

/// The cost of each kind of device asked for, and how many.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KindCost {
    pub platform: Platform,
    pub form_factor: Option<FormFactor>,
    pub count: usize,
    #[serde(flatten)]
    pub cost: Cost,
}

/// A value now and after the new devices boot.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Span {
    pub now: f64,
    pub after: f64,
}

/// The 1-minute load now and after, against the verdict's limit.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct LoadSpan {
    pub now: f64,
    pub after: f64,
    pub limit: f64,
}

/// What the new devices would do to the host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Predicted {
    /// "3 more Android phone devices".
    pub subject: String,
    pub devices: usize,
    /// How many of them (in order) the host admits now.
    pub admissible: usize,
    pub costs: Vec<KindCost>,
    /// At the worst: at most the call's `concurrency` of them booting at once, the rest idle.
    pub boot_cpu_pct: f64,
    pub idle_cpu_pct: f64,
    pub boot_memory_bytes: u64,
    pub idle_memory_bytes: u64,
    /// Percent of memory used now and with the new devices booting; `None` where unread.
    pub memory_used_pct: Option<Span>,
    /// Available bytes now and after (Linux); `None` on macOS.
    pub memory_available_bytes: Option<Span>,
    pub load: Option<LoadSpan>,
}

/// The verdict on starting new devices, every reason, and the prediction behind it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Admission {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub predicted: Predicted,
    /// Set when the host's history was short (a sampler that had been idle): says which readings
    /// the verdict used instead of the 5 and 15 minute averages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_basis: Option<String>,
}

/// The verdict's name: `admit`, `hold` or `backoff`.
pub fn verdict_name(v: Verdict) -> &'static str {
    match v {
        Verdict::Admit => "admit",
        Verdict::Hold => "hold",
        Verdict::Backoff => "backoff",
    }
}

fn rank(v: Verdict) -> u8 {
    match v {
        Verdict::Admit => 0,
        Verdict::Hold => 1,
        Verdict::Backoff => 2,
    }
}

/// "3 more Android phone devices", "1 more iOS device" (form factor unknown), "4 more devices".
pub fn subject(items: &[Item]) -> String {
    let n = items.len();
    let s = if n == 1 { "" } else { "s" };
    match items.first() {
        None => "no new device".to_owned(),
        Some(first) if items.iter().all(|i| i == first) => match first.form_factor {
            Some(f) => format!("{n} more {} {} device{s}", first.platform.label(), f.as_str()),
            None => format!("{n} more {} device{s}", first.platform.label()),
        },
        Some(_) => format!("{n} more devices"),
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Sums and judges `costs` on `host`: the verdict on the prediction alone, its reasons, and the numbers.
struct Judged {
    verdict: Verdict,
    reasons: Vec<String>,
    used: Option<Span>,
    avail: Option<Span>,
    load: Option<LoadSpan>,
}

/// What `costs` take at their worst: at most `concurrency` of them (the first) boot at once, the
/// rest are already idle by then. Bytes and percent of one core.
fn peak(costs: &[&Cost], concurrency: usize) -> (u64, f64) {
    let (booting, idle) = costs.split_at(costs.len().min(concurrency.max(1)));
    let mem = booting.iter().map(|c| c.boot_memory_bytes).sum::<u64>() + idle.iter().map(|c| c.idle_memory_bytes).sum::<u64>();
    let cpu = booting.iter().map(|c| c.boot_cpu_pct).sum::<f64>() + idle.iter().map(|c| c.idle_cpu_pct).sum::<f64>();
    (mem, cpu)
}

fn judge(host: &HostState, subject: &str, costs: &[&Cost], concurrency: usize, limits: &Limits) -> Judged {
    let (boot_mem, boot_cpu) = peak(costs, concurrency);
    let each = match costs.first() {
        _ if costs.len() > concurrency => format!(" (at most {concurrency} booting at once)"),
        Some(c) if costs.iter().all(|x| x.boot_memory_bytes == c.boot_memory_bytes) => {
            format!(" ({:.1} GB each while booting)", c.boot_memory_bytes as f64 / 1e9)
        }
        _ => String::new(),
    };
    let used = host.memory_total_bytes.filter(|t| *t > 0).zip(host.memory_free_pct).map(|(total, free)| {
        let now = 100.0 - free;
        Span { now: round1(now), after: round1(now + boot_mem as f64 / total as f64 * 100.0) }
    });
    let avail = host.memory_available_bytes.map(|a| Span { now: a as f64, after: (a as f64 - boot_mem as f64).max(0.0) });
    let load = host.load1.map(|l| LoadSpan { now: round2(l), after: round2(l + boot_cpu / 100.0), limit: host.now.load_limit });
    let (mut back, mut hold) = (Vec::new(), Vec::new());
    if let Some(u) = used {
        let line = |lim: f64, what: &str| {
            format!(
                "memory: {subject} would take memory used from {:.0}% to {:.0}%{each}, at or above the {what} limit {lim:.0}%",
                u.now, u.after
            )
        };
        if u.after >= limits.backoff_memory_used_pct {
            back.push(line(limits.backoff_memory_used_pct, "backoff"));
        } else if u.after >= limits.hold_memory_used_pct {
            hold.push(line(limits.hold_memory_used_pct, "hold"));
        }
    }
    if let Some(a) = avail {
        let (now, after) = (a.now / GIB, a.after / GIB);
        let line = |what: &str, lim: f64| {
            format!("memory: {subject} would leave {after:.1} GiB available (from {now:.1} GiB), below the {what} {lim:.1} GiB")
        };
        if after < limits.host.backoff_free_gb {
            back.push(line("backoff threshold", limits.host.backoff_free_gb));
        } else if after < limits.host.min_free_gb {
            hold.push(line("minimum", limits.host.min_free_gb));
        }
    }
    if let Some(l) = load.filter(|l| l.after >= l.limit) {
        hold.push(format!(
            "load: {subject} would take the 1-minute load from {:.2} to {:.2} while booting, at or above the limit {:.2}",
            l.now, l.after, l.limit
        ));
    }
    let (verdict, reasons) = if !back.is_empty() {
        back.extend(hold);
        (Verdict::Backoff, back)
    } else if !hold.is_empty() {
        (Verdict::Hold, hold)
    } else {
        let mut parts = Vec::new();
        if let Some(u) = used {
            parts.push(format!("memory used from {:.0}% to {:.0}%", u.now, u.after));
        }
        if let Some(l) = load {
            parts.push(format!("the 1-minute load from {:.2} to {:.2} (limit {:.2})", l.now, l.after, l.limit));
        }
        let what = if parts.is_empty() { "nothing this host can measure".to_owned() } else { parts.join(" and ") };
        (Verdict::Admit, vec![format!("{subject} would take {what}")])
    };
    Judged { verdict, reasons, used, avail, load }
}

/// The verdict on starting `items` now: the prediction judged on `host` (memory used against
/// [`Limits`], available memory and load against the host thresholds), and the host's own verdict
/// (thermal, power, pressure), whichever is worse, with every reason. At most `concurrency` of the
/// items boot at once (`boot_many`'s), so the rest cost what they do idle, not what they do booting.
pub fn admit(
    host: &HostState,
    items: &[Item],
    concurrency: usize,
    cost: impl Fn(Platform, FormFactor) -> Cost,
    limits: &Limits,
) -> Admission {
    let each: Vec<Cost> = items.iter().map(|i| cost(i.platform, i.form_factor.unwrap_or(FormFactor::Phone))).collect();
    let mut costs: Vec<KindCost> = Vec::new();
    for (i, c) in items.iter().zip(&each) {
        match costs.iter_mut().find(|k| k.platform == i.platform && k.form_factor == i.form_factor) {
            Some(k) => k.count += 1,
            None => {
                let mut c = c.clone();
                if i.form_factor.is_none() {
                    c.source.push_str("; the form factor is unknown, so a phone's numbers");
                }
                costs.push(KindCost { platform: i.platform, form_factor: i.form_factor, count: 1, cost: c });
            }
        }
    }
    let subject = subject(items);
    let refs: Vec<&Cost> = each.iter().collect();
    let host_blocks = host.now.verdict != Verdict::Admit;
    let Judged { mut verdict, mut reasons, used, avail, load } = if items.is_empty() {
        Judged { verdict: Verdict::Admit, reasons: vec!["no new device: nothing to admit".to_owned()], used: None, avail: None, load: None }
    } else {
        judge(host, &subject, &refs, concurrency, limits)
    };
    if !items.is_empty() && host_blocks {
        if verdict == Verdict::Admit {
            reasons.clear();
        }
        reasons.extend(host.now.reasons.iter().cloned());
        if rank(host.now.verdict) > rank(verdict) {
            verdict = host.now.verdict;
        }
    }
    let admissible = if host_blocks {
        0
    } else {
        (1..=items.len()).take_while(|k| judge(host, &subject, &refs[..*k], concurrency, limits).verdict == Verdict::Admit).count()
    };
    Admission {
        host_basis: host.basis.clone(),
        verdict,
        reasons,
        predicted: Predicted {
            subject,
            devices: items.len(),
            admissible: if items.is_empty() { 0 } else { admissible },
            boot_cpu_pct: round1(peak(&refs, concurrency).1),
            idle_cpu_pct: round1(each.iter().map(|c| c.idle_cpu_pct).sum()),
            boot_memory_bytes: peak(&refs, concurrency).0,
            idle_memory_bytes: each.iter().map(|c| c.idle_memory_bytes).sum(),
            costs,
            memory_used_pct: used,
            memory_available_bytes: avail,
            load,
        },
    }
}

/// Time, for [`pace`]: the system clock, or a test's.
pub trait Clock {
    /// Time since the pacing began.
    fn elapsed(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// The system's monotonic clock.
pub struct SystemClock(Instant);

impl SystemClock {
    pub fn start() -> SystemClock {
        SystemClock(Instant::now())
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// How long [`pace`] waits for the host to admit work, and how often it asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wait {
    pub timeout: Duration,
    pub poll: Duration,
}

impl Wait {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
    pub const POLL: Duration = Duration::from_secs(5);
}

/// How often [`pace`] looks at its cancel flag while it waits.
const CANCEL_SLICE: Duration = Duration::from_millis(250);

/// One thing [`pace`] did: started `started` items, or (`started` 0) waited on the verdict.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    pub at_s: f64,
    pub started: usize,
    /// Items not started yet after this step.
    pub waiting: usize,
    pub verdict: Verdict,
    pub reason: String,
}

/// What [`pace`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct Paced<R> {
    /// What each started chunk returned, in order.
    pub results: Vec<R>,
    /// Each start, and each change of what it waited on.
    pub steps: Vec<Step>,
    /// Time spent waiting for the host to admit work (not the time the started chunks took).
    pub waited_s: f64,
    pub timed_out: bool,
    /// The caller cancelled: nothing more was started.
    pub cancelled: bool,
    /// Items never started (the timeout or a cancel came first): the last ones.
    pub not_started: usize,
    /// The last verdict it judged by.
    pub last: Option<Admission>,
}

/// Starts `items` in order, in chunks the host admits: `check` judges the items not started yet,
/// and as many as its `admissible` (counting only those `counts` says are new devices: one
/// already running costs nothing) go to `start` together. When none may start, it waits `poll`
/// and asks again, until it has waited `timeout` in all (the time `start` takes doesn't count);
/// `progress` hears every judgement. `cancelled` is asked before each judgement and between short
/// slices of every wait: once it says yes, nothing more starts.
#[allow(clippy::too_many_arguments)]
pub fn pace<T, R>(
    items: &[T],
    counts: impl Fn(&T) -> bool,
    clock: &dyn Clock,
    wait: Wait,
    cancelled: impl Fn() -> bool,
    mut check: impl FnMut(&[T]) -> Admission,
    mut start: impl FnMut(&[T]) -> R,
    mut progress: impl FnMut(&Step),
) -> Paced<R> {
    let (mut next, mut results, mut steps, mut waited, mut timed_out, mut was_cancelled, mut last) =
        (0, Vec::new(), Vec::<Step>::new(), Duration::ZERO, false, false, None);
    while next < items.len() {
        if cancelled() {
            was_cancelled = true;
            break;
        }
        let rest = &items[next..];
        let a = check(rest);
        let mut take = 0;
        let mut counted = 0;
        for t in rest {
            if counts(t) {
                if counted == a.predicted.admissible {
                    break;
                }
                counted += 1;
            }
            take += 1;
        }
        let step = Step {
            at_s: round1(clock.elapsed().as_secs_f64()),
            started: take,
            waiting: rest.len() - take,
            verdict: a.verdict,
            reason: a.reasons.first().cloned().unwrap_or_default(),
        };
        progress(&step);
        if take > 0 {
            steps.push(step);
            results.push(start(&rest[..take]));
            next += take;
            last = Some(a);
            continue;
        }
        // One line per change of what it waits on, not per poll.
        if steps.last().is_none_or(|s| s.started > 0 || s.verdict != step.verdict || s.reason != step.reason) {
            steps.push(step);
        }
        last = Some(a);
        let left = wait.timeout.saturating_sub(waited);
        if left.is_zero() {
            timed_out = true;
            break;
        }
        let mut nap = wait.poll.min(left);
        while !nap.is_zero() && !was_cancelled {
            let slice = nap.min(CANCEL_SLICE);
            clock.sleep(slice);
            waited += slice;
            nap -= slice;
            was_cancelled = cancelled();
        }
        if was_cancelled {
            break;
        }
    }
    Paced {
        results,
        steps,
        waited_s: round1(waited.as_secs_f64()),
        timed_out,
        cancelled: was_cancelled,
        not_started: items.len() - next,
        last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::{MemReading, Reading, decide};
    use crate::{Level, ThermalState};
    use std::cell::Cell;

    const GB: u64 = 1 << 30;

    /// A host with `total_gib` of memory, `free_pct` of it free (and, on "Linux", that much
    /// available), `cpus` CPUs at `load1`, and the host verdict those make (plus `thermal`).
    fn host(total_gib: u64, free_pct: f64, linux: bool, cpus: usize, load1: f64, thermal: Option<ThermalState>) -> HostState {
        let total = total_gib * GB;
        let avail = linux.then(|| (total as f64 * free_pct / 100.0) as u64);
        let reading = Reading {
            load1: Some(load1),
            cpus,
            mem: Some(MemReading { available_bytes: avail, level: Level::Normal }),
            thermal,
            power: None,
        };
        HostState {
            cpus,
            load1: Some(load1),
            memory_total_bytes: Some(total),
            memory_available_bytes: avail,
            memory_free_pct: Some(free_pct),
            now: decide(&reading, &Thresholds::default()),
            basis: None,
        }
    }

    fn items(n: usize, platform: Platform, f: Option<FormFactor>) -> Vec<Item> {
        vec![Item { platform, form_factor: f }; n]
    }

    fn judged(h: &HostState, it: &[Item]) -> Admission {
        admit(h, it, usize::MAX, seed, &Limits::default())
    }

    #[test]
    fn seeds_are_the_published_numbers() {
        let a = seed(Platform::Android, FormFactor::Phone);
        assert_eq!((a.idle_cpu_pct, a.boot_cpu_pct, a.idle_memory_bytes, a.boot_memory_bytes), (5.9, 250.0, 2800 * MB, 3764 * MB));
        let i = seed(Platform::Ios, FormFactor::Phone);
        assert_eq!((i.idle_cpu_pct, i.boot_cpu_pct, i.idle_memory_bytes, i.boot_memory_bytes), (10.0, 280.0, 2176 * MB, 2176 * MB));
        assert!(a.source.contains("docs/benchmarks.md") && !a.source.contains("isn't measured"));
        let watch = seed(Platform::Ios, FormFactor::Watch);
        assert_eq!(watch.boot_memory_bytes, i.boot_memory_bytes);
        assert!(watch.source.contains("a watch isn't measured yet"), "{}", watch.source);
    }

    #[test]
    fn the_verdict_table() {
        use FormFactor::Phone;
        use Platform::{Android, Ios};
        let mac64 = host(64, 59.0, false, 16, 2.0, None);
        // (host, items, verdict, admissible, a fragment of the first reason)
        let cases: Vec<(HostState, Vec<Item>, Verdict, usize, &str)> = vec![
            // 3 x 3.764 GB of 68.7 GB: 41% to 57%; load 2 + 7.5 = 9.5 under 24.
            (
                mac64.clone(),
                items(3, Android, Some(Phone)),
                Verdict::Admit,
                3,
                "3 more Android phone devices would take memory used from 41% to 57%",
            ),
            (mac64.clone(), items(3, Android, Some(Phone)), Verdict::Admit, 3, "the 1-minute load from 2.00 to 9.50 (limit 24.00)"),
            // 32 GiB: each phone is 11% of it: 42% to 86% at 4 (hold at 85), 97% at 5 (backoff at 95).
            (host(32, 58.0, false, 16, 2.0, None), items(3, Android, Some(Phone)), Verdict::Admit, 3, "from 42% to 75%"),
            (
                host(32, 58.0, false, 16, 2.0, None),
                items(4, Android, Some(Phone)),
                Verdict::Hold,
                3,
                "memory: 4 more Android phone devices would take memory used from 42% to 86% (3.8 GB each while booting), at or above the hold limit 85%",
            ),
            (
                host(32, 58.0, false, 16, 2.0, None),
                items(5, Android, Some(Phone)),
                Verdict::Backoff,
                3,
                "at or above the backoff limit 95%",
            ),
            // Load: 4 CPUs at 2.0, limit 6.0; one phone adds 2.5 (4.5), two 5.0 (7.0).
            (host(64, 59.0, false, 4, 2.0, None), items(1, Android, Some(Phone)), Verdict::Admit, 1, "load from 2.00 to 4.50"),
            (
                host(64, 59.0, false, 4, 2.0, None),
                items(2, Android, Some(Phone)),
                Verdict::Hold,
                1,
                "load: 2 more Android phone devices would take the 1-minute load from 2.00 to 7.00 while booting, at or above the limit 6.00",
            ),
            // Linux knows available bytes: 16 GiB at 75% free is 12 GiB; one phone leaves 8.5, two 5.0, three 1.5.
            (host(16, 75.0, true, 16, 1.0, None), items(1, Android, Some(Phone)), Verdict::Admit, 1, "from 25% to 47%"),
            (
                host(16, 75.0, true, 16, 1.0, None),
                items(2, Android, Some(Phone)),
                Verdict::Hold,
                1,
                "would leave 5.0 GiB available (from 12.0 GiB), below the minimum 8.0 GiB",
            ),
            (
                host(16, 75.0, true, 16, 1.0, None),
                items(3, Android, Some(Phone)),
                Verdict::Backoff,
                1,
                "below the backoff threshold 4.0 GiB",
            ),
            // iOS: 2.176 GB each.
            (
                host(16, 70.0, false, 16, 1.0, None),
                items(2, Ios, Some(Phone)),
                Verdict::Admit,
                2,
                "2 more iOS phone devices would take memory used from 30% to 55%",
            ),
            // The host's own verdict holds: nothing is admissible, and its reason is listed.
            (host(64, 59.0, false, 16, 2.0, Some(ThermalState::Serious)), items(1, Android, Some(Phone)), Verdict::Hold, 0, "thermal: "),
            (host(64, 59.0, false, 16, 2.0, Some(ThermalState::Critical)), items(1, Ios, Some(Phone)), Verdict::Backoff, 0, "thermal: "),
            // An unknown form factor costs a phone; nothing asked for is admitted.
            (mac64.clone(), items(1, Ios, None), Verdict::Admit, 1, "1 more iOS device would take"),
            (mac64.clone(), vec![], Verdict::Admit, 0, "no new device"),
        ];
        for (h, it, want, admissible, frag) in cases {
            let a = judged(&h, &it);
            assert_eq!((a.verdict, a.predicted.admissible), (want, admissible), "{} on {h:?}: {:?}", a.predicted.subject, a.reasons);
            assert!(a.reasons.iter().any(|r| r.contains(frag)), "{:?} should contain {frag:?}", a.reasons);
            assert_eq!(a.predicted.devices, it.len());
        }
    }

    #[test]
    fn the_prediction_has_the_numbers() {
        let mut it = items(2, Platform::Android, Some(FormFactor::Phone));
        it.push(Item { platform: Platform::Ios, form_factor: Some(FormFactor::Tablet) });
        let a = judged(&host(64, 59.0, true, 16, 2.0, None), &it);
        let p = &a.predicted;
        assert_eq!(p.subject, "3 more devices");
        assert_eq!(
            p.costs.iter().map(|k| (k.platform, k.form_factor, k.count)).collect::<Vec<_>>(),
            [(Platform::Android, Some(FormFactor::Phone), 2), (Platform::Ios, Some(FormFactor::Tablet), 1)]
        );
        assert_eq!((p.boot_cpu_pct, p.idle_cpu_pct), (780.0, 21.8));
        assert_eq!((p.boot_memory_bytes, p.idle_memory_bytes), ((2 * 3764 + 2176) * MB, (2 * 2800 + 2176) * MB));
        assert_eq!(p.load, Some(LoadSpan { now: 2.0, after: 9.8, limit: 24.0 }));
        let avail = p.memory_available_bytes.expect("Linux");
        assert_eq!(avail.now - avail.after, p.boot_memory_bytes as f64);
        assert_eq!(a.verdict, Verdict::Admit);
        assert!(p.costs[1].cost.source.contains("a tablet isn't measured yet"), "{}", p.costs[1].cost.source);
        assert_eq!(p.memory_used_pct.map(|u| u.now), Some(41.0));
    }

    #[test]
    fn the_host_estimate_blends_in_idle_costs_after_a_few_samples() {
        let mut e = Estimator::default();
        let (ios, phone) = (Platform::Ios, FormFactor::Phone);
        assert_eq!(e.cost(ios, phone), seed(ios, phone), "nothing seen: the seed");
        // Busy (maybe booting) and unrated devices don't count.
        e.observe(ios, phone, Some(300.0), 900 * MB);
        e.observe(ios, phone, None, 900 * MB);
        assert_eq!(e.cost(ios, phone), seed(ios, phone));
        // A slim-profile device (1158 MB idle at 3% of a core) seen once or twice doesn't replace the seed.
        for _ in 1..MIN_SAMPLES {
            e.observe(ios, phone, Some(3.0), 1158 * MB);
            assert_eq!(e.cost(ios, phone), seed(ios, phone), "too few samples to trust");
        }
        // From MIN_SAMPLES on it is the seed blended a fifth of the way per sample towards the device.
        e.observe(ios, phone, Some(3.0), 1158 * MB);
        let c = e.cost(ios, phone);
        let blend = |seed: f64, seen: f64| seen + (seed - seen) * (1.0 - WEIGHT).powi(MIN_SAMPLES as i32);
        let mem = blend(2176.0, 1158.0) * 1e6;
        assert!((c.idle_memory_bytes as f64 - mem).abs() < 1e3, "{} vs {mem}", c.idle_memory_bytes);
        assert!(c.idle_memory_bytes > 1158 * MB && c.idle_memory_bytes < 2176 * MB, "between the device and the seed");
        assert_eq!(c.boot_memory_bytes, c.idle_memory_bytes, "iOS boots at its idle footprint");
        assert!((c.idle_cpu_pct - (blend(10.0, 3.0) * 10.0).round() / 10.0).abs() < 1e-9, "{}", c.idle_cpu_pct);
        assert_eq!(c.boot_cpu_pct, 280.0);
        assert!(c.source.starts_with("this host: 3 samples of idle iOS phone devices blended with"), "{}", c.source);
        // Many samples converge on what the host measures.
        for _ in 0..40 {
            e.observe(ios, phone, Some(3.0), 1158 * MB);
        }
        assert!(e.cost(ios, phone).idle_memory_bytes.abs_diff(1158 * MB) < 5 * MB);
        // Android keeps its boot/idle memory ratio; kinds are separate.
        for _ in 0..MIN_SAMPLES {
            e.observe(Platform::Android, phone, Some(4.0), 1400 * MB);
        }
        let a = e.cost(Platform::Android, phone);
        assert!((a.boot_memory_bytes as f64 / a.idle_memory_bytes as f64 - 3764.0 / 2800.0).abs() < 1e-3);
        assert_eq!(e.cost(ios, FormFactor::Tablet), seed(ios, FormFactor::Tablet));
    }

    #[test]
    fn at_most_k_devices_boot_at_once_and_the_rest_cost_their_idle() {
        use FormFactor::Phone;
        use Platform::Android;
        // 16 CPUs at load 2 (limit 24), 256 GiB: 20 phones at 2.5 booting cores each.
        let h = host(256, 80.0, false, 16, 2.0, None);
        let twenty = items(20, Android, Some(Phone));
        let all_at_once = judged(&h, &twenty);
        assert_eq!((all_at_once.predicted.admissible, all_at_once.verdict), (8, Verdict::Hold), "N x boot: 2 + 8 x 2.5 = 22");
        // At most 4 at a time: 4 x 2.5 booting + 16 x 0.059 idle = 10.94 over the load of 2.
        let four = admit(&h, &twenty, 4, seed, &Limits::default());
        assert_eq!((four.predicted.admissible, four.verdict), (20, Verdict::Admit), "{:?}", four.reasons);
        let load = four.predicted.load.expect("load");
        assert_eq!((load.now, load.after), (2.0, 12.94));
        assert_eq!(four.predicted.boot_memory_bytes, 4 * 3764 * MB + 16 * 2800 * MB);
        assert_eq!(four.predicted.boot_cpu_pct, 1094.4, "{}", four.predicted.boot_cpu_pct);
        // Fewer devices than slots cost what they do booting, as before.
        let two = admit(&h, &items(2, Android, Some(Phone)), 4, seed, &Limits::default());
        assert_eq!(two.predicted.boot_memory_bytes, 2 * 3764 * MB);
        // Memory: 32 GiB at 42% used, 6 phones with K = 2: 2 x 3.764 + 4 x 2.8 = 18.7 GB = 54% more.
        let small = host(32, 58.0, false, 16, 2.0, None);
        let six = admit(&small, &items(6, Android, Some(Phone)), 2, seed, &Limits::default());
        assert_eq!(six.predicted.memory_used_pct.map(|u| u.after), Some(96.5), "{:?}", six.reasons);
        assert_eq!(six.verdict, Verdict::Backoff);
        assert!(six.reasons[0].contains("at most 2 booting at once"), "{:?}", six.reasons);
    }

    struct FakeClock(Cell<Duration>);

    impl Clock for FakeClock {
        fn elapsed(&self) -> Duration {
            self.0.get()
        }

        fn sleep(&self, d: Duration) {
            self.0.set(self.0.get() + d);
        }
    }

    /// An admission whose `admissible` is `n` (capped at what was asked).
    fn allowing(n: usize, rest: usize) -> Admission {
        let mut a = judged(&host(64, 59.0, false, 16, 1.0, None), &items(rest, Platform::Android, Some(FormFactor::Phone)));
        a.predicted.admissible = n.min(rest);
        if n < rest {
            a.verdict = Verdict::Hold;
            a.reasons = vec![format!("memory: room for {n}")];
        }
        a
    }

    #[test]
    fn pacing_starts_chunks_as_the_host_admits_them() {
        // Room for none until 10 s, two until 30 s, then every one; each started one takes a place.
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let running = Cell::new(0usize);
        let room = |t: Duration| -> usize {
            match t.as_secs() {
                0..10 => 0,
                10..30 => 2,
                _ => 10,
            }
        };
        let wait = Wait { timeout: Duration::from_secs(600), poll: Duration::from_secs(5) };
        let mut heard = 0;
        let paced = pace(
            &[1, 2, 3, 4, 5],
            |_| true,
            &clock,
            wait,
            || false,
            |rest| allowing(room(clock.elapsed()).saturating_sub(running.get()), rest.len()),
            |chunk: &[i32]| {
                running.set(running.get() + chunk.len());
                chunk.to_vec()
            },
            |_| heard += 1,
        );
        assert_eq!(paced.results, [vec![1, 2], vec![3, 4, 5]]);
        assert_eq!((paced.waited_s, paced.timed_out, paced.not_started), (30.0, false, 0));
        let steps: Vec<(f64, usize, usize, Verdict)> = paced.steps.iter().map(|s| (s.at_s, s.started, s.waiting, s.verdict)).collect();
        // Waits are one line per change of reason: at 0 (room for 0), 10 (room for 0 left after two).
        assert_eq!(
            steps,
            [(0.0, 0, 5, Verdict::Hold), (10.0, 2, 3, Verdict::Hold), (10.0, 0, 3, Verdict::Hold), (30.0, 3, 0, Verdict::Admit)]
        );
        assert_eq!(heard, 8, "every judgement: at 0 and 5 s, the start at 10, the waits at 10, 15, 20 and 25, the start at 30");
        assert_eq!(paced.last.map(|a| a.verdict), Some(Verdict::Admit));
    }

    #[test]
    fn pacing_stops_at_the_timeout_and_running_devices_need_no_room() {
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let wait = Wait { timeout: Duration::from_secs(12), poll: Duration::from_secs(5) };
        // Items 0 are already running (no cost): they start at once; the new ones never get room.
        let paced = pace(&[0, 0, 1, 1], |t| *t == 1, &clock, wait, || false, |rest| allowing(0, rest.len()), |c: &[i32]| c.len(), |_| {});
        assert_eq!(paced.results, [2]);
        assert_eq!((paced.timed_out, paced.not_started, paced.waited_s), (true, 2, 12.0), "slept 5, 5 and the 2 s left");
        assert_eq!(clock.elapsed(), Duration::from_secs(12));
        assert_eq!(paced.steps.last().map(|s| (s.started, s.waiting, s.reason.as_str())), Some((0, 2, "memory: room for 0")));
        // A zero timeout judges once and doesn't sleep.
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let none = Wait { timeout: Duration::ZERO, poll: Duration::from_secs(5) };
        let paced = pace(&[1], |_| true, &clock, none, || false, |rest| allowing(0, rest.len()), |c: &[i32]| c.len(), |_| {});
        assert_eq!((paced.results.len(), paced.timed_out, clock.elapsed()), (0, true, Duration::ZERO));
    }

    #[test]
    fn the_wait_budget_is_time_spent_waiting_not_time_spent_booting() {
        // Room for one now; the chunk takes 600 s to boot (more than the whole budget); then the
        // host is full for 20 s more. Only those 20 s count against the 30 s.
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let booted = Cell::new(None::<Duration>);
        let wait = Wait { timeout: Duration::from_secs(30), poll: Duration::from_secs(5) };
        let paced = pace(
            &[1, 2],
            |_| true,
            &clock,
            wait,
            || false,
            |rest| match booted.get() {
                None => allowing(1, rest.len()),
                Some(t) if clock.elapsed() >= t + Duration::from_secs(20) => allowing(rest.len(), rest.len()),
                Some(_) => allowing(0, rest.len()),
            },
            |chunk: &[i32]| {
                clock.sleep(Duration::from_secs(600));
                booted.set(Some(clock.elapsed()));
                chunk.len()
            },
            |_| {},
        );
        assert_eq!((paced.results.clone(), paced.timed_out, paced.not_started), (vec![1, 1], false, 0));
        assert_eq!(paced.waited_s, 20.0, "only the waiting");
        // Out of budget: the time reported is the time waited, whatever the chunks took.
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let paced = pace(
            &[1, 2],
            |_| true,
            &clock,
            wait,
            || false,
            |rest| if clock.elapsed() < Duration::from_secs(600) { allowing(1, rest.len()) } else { allowing(0, rest.len()) },
            |chunk: &[i32]| {
                clock.sleep(Duration::from_secs(600));
                chunk.len()
            },
            |_| {},
        );
        assert_eq!((paced.results.len(), paced.timed_out, paced.not_started, paced.waited_s), (1, true, 1, 30.0));
    }

    #[test]
    fn a_cancel_stops_the_wait_between_polls_and_starts_nothing_more() {
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let wait = Wait { timeout: Duration::from_secs(600), poll: Duration::from_secs(5) };
        let started = Cell::new(0usize);
        let cancel_at = Duration::from_secs(7);
        let paced = pace(
            &[1, 2, 3],
            |_| true,
            &clock,
            wait,
            || clock.elapsed() >= cancel_at,
            |rest| allowing(0, rest.len()),
            |c: &[i32]| {
                started.set(started.get() + c.len());
                c.len()
            },
            |_| {},
        );
        assert!(paced.cancelled && !paced.timed_out);
        assert_eq!((started.get(), paced.not_started), (0, 3));
        assert!(
            clock.elapsed() < Duration::from_secs(8),
            "stopped within a slice of the flag, not at the next 5 s poll or the timeout: {:?}",
            clock.elapsed()
        );
        // Cancelled before the first judgement: nothing judged, nothing started.
        let paced = pace(&[1], |_| true, &clock, wait, || true, |_| panic!("no judgement"), |_: &[i32]| panic!("no start"), |_| {});
        assert!(paced.cancelled && paced.last.is_none());
        // Cancelled after one chunk: the rest never start.
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let first = Cell::new(false);
        let paced = pace(
            &[1, 2, 3],
            |_| true,
            &clock,
            wait,
            || first.get(),
            |_| allowing(1, 3),
            |c: &[i32]| {
                first.set(true);
                c.len()
            },
            |_| {},
        );
        assert_eq!((paced.results.len(), paced.not_started, paced.cancelled), (1, 2, true));
    }
}
