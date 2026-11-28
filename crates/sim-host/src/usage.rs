//! Per-device load attribution: how much of the host each running device uses (an iOS simulator,
//! simee's Android emulator `emu`, the stock Android Emulator), and how much simee's own processes
//! use. CPU over the last second and minute, resident memory and footprint, disk I/O, and energy
//! where the kernel reports it.
//!
//! One pass over the process table per sample, no process spawned: the table (pid, parent, short
//! name) from libproc, the arguments of the few processes that name a device (read once per
//! process, by pid and start time), and one `proc_pid_rusage` per attributed process. CPU, disk
//! I/O and energy are accumulated counters, so a window's rate is the sum of the deltas between
//! samples over the time they span: a call never sleeps to measure.
//!
//! Which process belongs to which device (spectra's `internal/hostload/devices.go` and
//! `internal/process/family.go`, MIT, same author):
//! - iOS: a device is its `launchd_sim` and every descendant; the UDID is the path segment its
//!   arguments name before `/data` (`.../Devices/<UDID>/data/var/run/launchd_bootstrap.plist`).
//! - simee's emulator: an `emu` started for an AVD (`-avd <name>`, `-avd=<name>` or `@<name>`) and
//!   its descendants (a software-mode qemu, helpers).
//! - The stock emulator: a `qemu-system-*` outside those trees that names an AVD, runs from the
//!   SDK's `emulator/qemu/` directory or has an `emulator` launcher ancestor (a bare qemu also
//!   backs colima, lima and podman machines, which aren't devices), with its descendants and its
//!   launcher ancestors (and theirs); a launcher with no qemu under it yet is a device of its own.
//! - simee: `simee` (the server, the CLI, the MCP server), `simee-desktop`, and an `emu` that runs
//!   no AVD (`-list-avds`, `-create-avd`), with their descendants that aren't a device's.
//!
//! A pid names one process only with its start time: a pid reused between two samples is a new
//! process, whose counters are never subtracted from the old one's. A process that started since
//! the last sample counts whole (all its CPU fell in the window); one seen for the first time
//! that started earlier (the first sample, a process that just joined a tree) is a baseline. A
//! process that exits between samples takes its last interval with it, except for the CPU its
//! parent reaped: a member's `ri_child_user_time + ri_child_system_time` grows by the whole CPU
//! time of each child it reaps, so the growth less what the exited members had used when last seen
//! is added back (children that came and went inside one interval included). Limits: a child
//! reparented out of the group (its parent exited first) is reaped by `launchd`, outside it, and
//! its last interval is lost; the recovered time lands in the interval it was reaped in.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;

/// Which emulator or simulator runs a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An iOS simulator (`launchd_sim`).
    Simulator,
    /// simee's Android emulator.
    Emu,
    /// The stock Android Emulator (qemu).
    Emulator,
}

impl Kind {
    /// The platform its devices run: `ios` or `android`.
    pub fn platform(self) -> &'static str {
        match self {
            Kind::Simulator => "ios",
            Kind::Emu | Kind::Emulator => "android",
        }
    }
}

/// One row of the process table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// The short name (`p_comm`, up to 16 bytes: `qemu-system-aarch64` is `qemu-system-aar`).
    pub name: String,
    /// argv (argv\[0\] first), read only for the processes [`wants_argv`] names; empty otherwise.
    pub argv: Vec<String>,
    /// The executable's own path (the `KERN_PROCARGS2` record's, which a bare argv\[0\] doesn't
    /// show), read with argv.
    pub exe: String,
}

/// Whether attribution reads the arguments of a process named `name`: the few that name a device.
pub fn wants_argv(name: &str) -> bool {
    matches!(name, "launchd_sim" | "emu" | "emulator") || is_qemu(name)
}

fn is_qemu(name: &str) -> bool {
    name.get(..12).is_some_and(|p| p.eq_ignore_ascii_case("qemu-system-"))
}

const SIMEE_NAMES: [&str; 2] = ["simee", "simee-desktop"];

/// A device's processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub kind: Kind,
    /// The simulator's UDID or the AVD's name; `<name> pid <pid>` when the process names neither.
    pub id: String,
    /// The `launchd_sim`, `emu` or qemu.
    pub root: i32,
    /// The root first, then its descendants (and a stock emulator's launchers).
    pub pids: Vec<i32>,
}

/// Every device's processes, and simee's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attribution {
    pub devices: Vec<Group>,
    pub simee: Vec<i32>,
}

/// Splits the process table by device (see the module's rules).
pub fn attribute(procs: &[Proc]) -> Attribution {
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut kids: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        if p.pid != p.ppid {
            kids.entry(p.ppid).or_default().push(p.pid);
        }
    }
    for v in kids.values_mut() {
        v.sort_unstable();
    }
    let mut sorted: Vec<&Proc> = procs.iter().collect();
    sorted.sort_by_key(|p| p.pid);
    let mut claimed: HashSet<i32> = HashSet::new();
    let claim = |root: i32, claimed: &mut HashSet<i32>| -> Vec<i32> {
        if !claimed.insert(root) {
            return Vec::new();
        }
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            for &c in kids.get(&out[i]).map_or(&[][..], Vec::as_slice) {
                if claimed.insert(c) {
                    out.push(c);
                }
            }
            i += 1;
        }
        out
    };
    // Parents, nearest first (a cycle from a pid reused mid-read ends the walk).
    let ancestors = |pid: i32| -> Vec<&Proc> {
        let mut out = Vec::new();
        let mut seen = HashSet::from([pid]);
        let mut cur = by_pid.get(&pid).map(|p| p.ppid);
        while let Some(pp) = cur.filter(|&pp| pp > 1 && seen.insert(pp)) {
            let Some(p) = by_pid.get(&pp) else { break };
            out.push(*p);
            cur = Some(p.ppid);
        }
        out
    };
    let launcher = |p: &Proc| p.name == "emulator";

    let mut devices = Vec::new();
    for p in sorted.iter().filter(|p| p.name == "launchd_sim") {
        let pids = claim(p.pid, &mut claimed);
        if !pids.is_empty() {
            let id = udid_in(&p.argv).unwrap_or_else(|| format!("launchd_sim pid {}", p.pid));
            devices.push(Group { kind: Kind::Simulator, id, root: p.pid, pids });
        }
    }
    for p in sorted.iter().filter(|p| p.name == "emu") {
        if let Some(avd) = avd_name(&p.argv).filter(|_| !claimed.contains(&p.pid)) {
            let pids = claim(p.pid, &mut claimed);
            devices.push(Group { kind: Kind::Emu, id: avd, root: p.pid, pids });
        }
    }
    // Stock qemus claim their own trees first, then their launchers (nearest first) claim
    // themselves and what else they started.
    let mut launchers_of: Vec<(usize, Vec<i32>)> = Vec::new();
    for p in sorted.iter().filter(|p| is_qemu(&p.name)) {
        if claimed.contains(&p.pid) {
            continue;
        }
        let launchers: Vec<&Proc> = ancestors(p.pid).into_iter().filter(|a| launcher(a)).collect();
        let own = avd_name(&p.argv);
        let from_sdk = p.exe.contains("/emulator/qemu/") || p.argv.iter().any(|a| a.contains("/emulator/qemu/"));
        if own.is_none() && !from_sdk && launchers.is_empty() {
            continue;
        }
        let avd = own.or_else(|| launchers.iter().find_map(|l| avd_name(&l.argv)));
        let pids = claim(p.pid, &mut claimed);
        launchers_of.push((devices.len(), launchers.iter().map(|l| l.pid).collect()));
        devices.push(Group { kind: Kind::Emulator, id: avd.unwrap_or_else(|| format!("{} pid {}", p.name, p.pid)), root: p.pid, pids });
    }
    for (i, launchers) in launchers_of {
        for l in launchers {
            let more = claim(l, &mut claimed);
            devices[i].pids.extend(more);
        }
    }
    for p in sorted.iter().filter(|p| launcher(p)) {
        // Claimed by its qemu, or an outer launcher's tree holds it.
        if claimed.contains(&p.pid) || ancestors(p.pid).iter().any(|a| launcher(a)) {
            continue;
        }
        let pids = claim(p.pid, &mut claimed);
        let id = avd_name(&p.argv).unwrap_or_else(|| format!("emulator pid {}", p.pid));
        devices.push(Group { kind: Kind::Emulator, id, root: p.pid, pids });
    }
    let mut simee = Vec::new();
    for p in sorted.iter().filter(|p| SIMEE_NAMES.contains(&p.name.as_str()) || (p.name == "emu" && avd_name(&p.argv).is_none())) {
        simee.extend(claim(p.pid, &mut claimed));
    }
    Attribution { devices, simee }
}

/// The AVD an emulator command line runs: `-avd NAME`, `-avd=NAME` or `@NAME` (after argv\[0\]).
pub fn avd_name(argv: &[String]) -> Option<String> {
    let args = argv.get(1..)?;
    args.iter().enumerate().find_map(|(i, a)| {
        if a == "-avd" {
            args.get(i + 1).filter(|n| !n.is_empty()).cloned()
        } else if let Some(n) = a.strip_prefix("-avd=") {
            (!n.is_empty()).then(|| n.to_owned())
        } else {
            a.strip_prefix('@').filter(|n| !n.is_empty()).map(str::to_owned)
        }
    })
}

/// The simulator UDID a `launchd_sim`'s arguments name: the UUID path segment before `data`.
pub fn udid_in(argv: &[String]) -> Option<String> {
    argv.iter().find_map(|a| {
        let segs: Vec<&str> = a.split('/').collect();
        segs.windows(2).find(|w| is_uuid(w[0]) && w[1] == "data").map(|w| w[0].to_owned())
    })
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() })
}

/// The executable path a `KERN_PROCARGS2` record carries before argv (empty when it has none).
pub fn exe_from_procargs(b: &[u8]) -> String {
    let rest = b.get(4..).unwrap_or_default();
    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).into_owned()
}

/// argv from a `KERN_PROCARGS2` record: `argc` (native `i32`), the executable path, NUL padding,
/// then `argc` NUL-terminated strings (the environment after them is left out).
pub fn argv_from_procargs(b: &[u8]) -> Vec<String> {
    let Some(argc) = b.get(..4).and_then(|x| x.try_into().ok()).map(i32::from_ne_bytes) else { return Vec::new() };
    let rest = &b[4..];
    let Some(exe_end) = rest.iter().position(|&c| c == 0) else { return Vec::new() };
    let mut i = exe_end;
    while rest.get(i) == Some(&0) {
        i += 1;
    }
    let mut out = Vec::new();
    while out.len() < argc.max(0) as usize && i < rest.len() {
        let end = rest[i..].iter().position(|&c| c == 0).map_or(rest.len(), |n| i + n);
        out.push(String::from_utf8_lossy(&rest[i..end]).into_owned());
        i = end + 1;
    }
    out
}

/// A process's accumulated counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Start time on the observation's clock (with the pid, it names one process).
    pub start_ns: u64,
    pub cpu_ns: u64,
    /// CPU time of the children this process has reaped (user and system, their own descendants
    /// folded in): it grows when a child exits, so it holds the CPU of ones that came and went
    /// between two samples.
    pub child_cpu_ns: u64,
    pub resident_bytes: u64,
    pub footprint_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_written_bytes: u64,
    pub energy_nj: Option<u64>,
}

/// One pass over the host: who belongs to which device and every attributed process's counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observation {
    /// When the pass began, on a monotonic clock in nanoseconds (the one start times are on).
    pub at_ns: u64,
    pub attribution: Attribution,
    pub counters: HashMap<i32, Counters>,
    /// The host's CPU ticks (user, system, idle, nice), 32-bit counters that wrap.
    pub host_ticks: Option<[u32; 4]>,
}

/// What a group used in one interval between samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Step {
    dur_ns: u64,
    cpu_ns: u64,
    read: u64,
    written: u64,
    energy_nj: Option<u64>,
}

impl Step {
    fn add(&mut self, o: &Step) {
        self.dur_ns += o.dur_ns;
        self.cpu_ns += o.cpu_ns;
        self.read += o.read;
        self.written += o.written;
        self.energy_nj = match (self.energy_nj, o.energy_nj) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
    }
}

/// The steps that cover `window_ns` best, newest first and summed; `None` before the second sample.
fn window(steps: &VecDeque<Step>, window_ns: u64) -> Option<Step> {
    let mut acc: Option<Step> = None;
    for s in steps.iter().rev() {
        match &mut acc {
            None => acc = Some(*s),
            // Take the next older step only when it brings the span closer to the window.
            Some(a) if (a.dur_ns + s.dur_ns).abs_diff(window_ns) < a.dur_ns.abs_diff(window_ns) => a.add(s),
            Some(_) => break,
        }
    }
    acc.filter(|a| a.dur_ns > 0)
}

const S1: u64 = 1_000_000_000;
const M1: u64 = 60 * S1;
/// History kept per group: a little over the longest window.
const KEEP_NS: u64 = M1 + 5 * S1;
const MAX_STEPS: usize = 4096;

fn push(steps: &mut VecDeque<Step>, s: Step) {
    steps.push_back(s);
    let mut total: u64 = steps.iter().map(|s| s.dur_ns).sum();
    while steps.len() > 1 && (steps.len() > MAX_STEPS || total - steps[0].dur_ns >= KEEP_NS) {
        total -= steps[0].dur_ns;
        steps.pop_front();
    }
}

/// A value over the last second and the last minute (over as much as there is, for a device that
/// started within the window); `None` before the second sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Rate {
    #[serde(rename = "1s")]
    pub s1: Option<f64>,
    #[serde(rename = "1m")]
    pub m1: Option<f64>,
}

impl Rate {
    fn of(steps: &VecDeque<Step>, f: impl Fn(&Step) -> Option<f64>) -> Rate {
        Rate { s1: window(steps, S1).and_then(|s| f(&s)), m1: window(steps, M1).and_then(|s| f(&s)) }
    }

    fn sum(rates: impl Iterator<Item = Rate>) -> Rate {
        let add = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        rates.fold(Rate::default(), |acc, r| Rate { s1: add(acc.s1, r.s1), m1: add(acc.m1, r.m1) })
    }

    fn share(part: Rate, whole: Rate) -> Rate {
        let pct = |p: Option<f64>, w: Option<f64>| Some(p? / w.filter(|w| *w > 0.0)? * 100.0);
        Rate { s1: pct(part.s1, whole.s1), m1: pct(part.m1, whole.m1) }
    }

    /// What sorts rows: the minute, else the second.
    fn cost(&self) -> f64 {
        self.m1.or(self.s1).unwrap_or(0.0)
    }
}

fn cpu_pct(s: &Step) -> Option<f64> {
    Some(s.cpu_ns as f64 / s.dur_ns as f64 * 100.0)
}

fn per_s(bytes: u64, s: &Step) -> Option<f64> {
    Some(bytes as f64 / (s.dur_ns as f64 / 1e9))
}

#[derive(Clone, Debug, Default)]
struct Series {
    id: String,
    /// The pids counted last sample, to see which exited since.
    members: Vec<i32>,
    steps: VecDeque<Step>,
    root: Option<i32>,
    pids: usize,
    resident: u64,
    footprint: u64,
    energy: bool,
}

/// Group keys: a device's kind, root pid and the root's start time (so several instances of one
/// AVD are several devices, and a reused pid a new one); simee's is `(None, 0, 0)`.
type Key = (Option<Kind>, i32, u64);

/// The CPU a group's exited children used since the last sample that the sampled deltas missed:
/// what its members reaped in the window (`child_delta`) less the CPU the exited members had when
/// last seen (`gone_seen`), which the previous deltas already counted.
fn recovered_child_cpu(child_delta: u64, gone_seen: u64) -> u64 {
    child_delta.saturating_sub(gone_seen)
}

/// Turns observations into per-device rates: keeps each attributed process's last counters and a
/// minute of per-group deltas.
#[derive(Clone, Debug, Default)]
pub struct Tracker {
    at_ns: Option<u64>,
    samples: u64,
    prev: HashMap<i32, Counters>,
    series: HashMap<Key, Series>,
    clock: VecDeque<Step>,
    host: VecDeque<Step>,
    host_ticks: Option<[u32; 4]>,
    cpus: usize,
}

impl Tracker {
    /// A tracker for a host with `cpus` CPUs (host CPU is reported as percent of one core, as
    /// device CPU is: 400 is four cores busy).
    pub fn new(cpus: usize) -> Tracker {
        Tracker { cpus: cpus.max(1), ..Tracker::default() }
    }

    /// Observations taken so far.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Adds one observation.
    pub fn update(&mut self, o: Observation) {
        let since = self.at_ns;
        let dur = since.map(|t| o.at_ns.saturating_sub(t)).filter(|d| *d > 0);
        let delta = |pid: i32| -> (Step, u64) {
            let Some(c) = o.counters.get(&pid) else { return (Step::default(), 0) };
            let base = match self.prev.get(&pid) {
                // The same process: what it used since.
                Some(p) if p.start_ns == c.start_ns => *p,
                // Started since the last sample (or a reused pid): all of it is new.
                _ if since.is_some_and(|t| c.start_ns >= t) => Counters { energy_nj: c.energy_nj.map(|_| 0), ..Counters::default() },
                // Seen for the first time, older than the last sample: a baseline.
                _ => *c,
            };
            let step = Step {
                dur_ns: 0,
                cpu_ns: c.cpu_ns.saturating_sub(base.cpu_ns),
                read: c.disk_read_bytes.saturating_sub(base.disk_read_bytes),
                written: c.disk_written_bytes.saturating_sub(base.disk_written_bytes),
                energy_nj: c.energy_nj.map(|e| e.saturating_sub(base.energy_nj.unwrap_or(e))),
            };
            (step, c.child_cpu_ns.saturating_sub(base.child_cpu_ns))
        };
        let start_of = |pid: i32| o.counters.get(&pid).map_or(0, |c| c.start_ns);
        // One AVD run several times is several devices: each shows as `<id> #<n>`, oldest first.
        let mut same: HashMap<(Kind, &str), Vec<(u64, i32)>> = HashMap::new();
        for g in &o.attribution.devices {
            same.entry((g.kind, g.id.as_str())).or_default().push((start_of(g.root), g.root));
        }
        for v in same.values_mut() {
            v.sort_unstable();
        }
        let label = |g: &Group| match same.get(&(g.kind, g.id.as_str())) {
            Some(v) if v.len() > 1 => {
                let n = v.iter().position(|&(_, r)| r == g.root).map_or(0, |i| i + 1);
                format!("{} #{n}", g.id)
            }
            _ => g.id.clone(),
        };
        let groups = o
            .attribution
            .devices
            .iter()
            .map(|g| ((Some(g.kind), g.root, start_of(g.root)), label(g), Some(g.root), &g.pids))
            .chain(std::iter::once(((None, 0, 0), "simee".to_owned(), None, &o.attribution.simee)));
        let mut seen = HashSet::new();
        for (key, id, root, pids) in groups {
            if !seen.insert(key) {
                continue;
            }
            let mut step = Step { dur_ns: dur.unwrap_or(0), ..Step::default() };
            let (mut n, mut resident, mut footprint, mut energy, mut child) = (0, 0, 0, false, 0u64);
            let mut members = Vec::new();
            for &pid in pids {
                let Some(c) = o.counters.get(&pid) else { continue };
                n += 1;
                members.push(pid);
                resident += c.resident_bytes;
                footprint += c.footprint_bytes;
                energy |= c.energy_nj.is_some();
                let (d, cd) = delta(pid);
                step.add(&d);
                child += cd;
            }
            let s = self.series.entry(key).or_default();
            // Members that exited since the last sample took their last interval with them; the
            // children their parents reaped carry it (a new group has no previous members).
            let gone_seen: u64 = s
                .members
                .iter()
                .filter_map(|pid| self.prev.get(pid).filter(|p| o.counters.get(pid).is_none_or(|c| c.start_ns != p.start_ns)))
                .map(|p| p.cpu_ns)
                .sum();
            step.cpu_ns += recovered_child_cpu(child, gone_seen);
            if dur.is_some() {
                push(&mut s.steps, step);
            }
            *s = Series { id, members, steps: std::mem::take(&mut s.steps), root, pids: n, resident, footprint, energy };
        }
        self.series.retain(|k, _| seen.contains(k));
        if let Some(d) = dur {
            push(&mut self.clock, Step { dur_ns: d, ..Step::default() });
            if let (Some(a), Some(b)) = (self.host_ticks, o.host_ticks) {
                let dt: Vec<u64> = (0..4).map(|i| u64::from(b[i].wrapping_sub(a[i]))).collect();
                let total: u64 = dt.iter().sum();
                if total > 0 {
                    let busy = (total - dt[2]) as f64 / total as f64;
                    push(&mut self.host, Step { dur_ns: d, cpu_ns: (busy * d as f64 * self.cpus as f64) as u64, ..Step::default() });
                }
            }
        }
        self.host_ticks = o.host_ticks.or(self.host_ticks);
        self.prev = o.counters;
        self.at_ns = Some(o.at_ns);
        self.samples += 1;
    }

    /// Every device's usage, costliest first, simee's, and the host's totals.
    pub fn report(&self) -> Report {
        let usage = |(kind, _, _): &Key, s: &Series| Usage {
            kind: *kind,
            id: s.id.clone(),
            pid: s.root,
            pids: s.pids,
            cpu_pct: Rate::of(&s.steps, cpu_pct),
            resident_bytes: s.resident,
            footprint_bytes: s.footprint,
            disk_read_bytes_per_s: Rate::of(&s.steps, |w| per_s(w.read, w)),
            disk_write_bytes_per_s: Rate::of(&s.steps, |w| per_s(w.written, w)),
            // nJ per ns is W.
            energy_w: s.energy.then(|| Rate::of(&s.steps, |w| Some(w.energy_nj? as f64 / w.dur_ns as f64))),
        };
        let mut devices: Vec<Usage> = self.series.iter().filter(|(k, _)| k.0.is_some()).map(|(k, s)| usage(k, s)).collect();
        devices.sort_by(|a, b| {
            b.cpu_pct.cost().total_cmp(&a.cpu_pct.cost()).then(b.footprint_bytes.cmp(&a.footprint_bytes)).then_with(|| a.id.cmp(&b.id))
        });
        let simee_key: Key = (None, 0, 0);
        let simee = self
            .series
            .get(&simee_key)
            .map_or_else(|| Usage { kind: None, id: "simee".into(), pid: None, pids: 0, ..Usage::empty() }, |s| usage(&simee_key, s));
        let host_cpu = Rate::of(&self.host, cpu_pct);
        let devices_cpu = Rate::sum(devices.iter().map(|d| d.cpu_pct));
        let host = Host {
            cpus: self.cpus,
            cpu_pct: host_cpu,
            memory_total_bytes: None,
            devices: devices.len(),
            devices_cpu_pct: devices_cpu,
            devices_resident_bytes: devices.iter().map(|d| d.resident_bytes).sum(),
            devices_footprint_bytes: devices.iter().map(|d| d.footprint_bytes).sum(),
            devices_share_pct: Rate::share(devices_cpu, host_cpu),
            simee_share_pct: Rate::share(simee.cpu_pct, host_cpu),
        };
        let span = |w| window(&self.clock, w).map(|s| s.dur_ns as f64 / 1e9);
        Report {
            available: AVAILABLE,
            reason: (!AVAILABLE).then(|| UNAVAILABLE.to_owned()),
            samples: self.samples,
            window_s: Rate { s1: span(S1), m1: span(M1) },
            devices,
            simee,
            host,
        }
    }
}

/// One device's (or simee's) usage.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    pub id: String,
    /// The device's `launchd_sim`, `emu` or qemu; none for simee.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    /// Processes counted.
    pub pids: usize,
    /// Percent of one core (Activity Monitor's % CPU): 250 is two and a half cores.
    pub cpu_pct: Rate,
    pub resident_bytes: u64,
    /// Physical footprint (Activity Monitor's Memory): dirty memory, compressed and swapped pages
    /// included. libproc reports no per-process compressed size of its own.
    pub footprint_bytes: u64,
    pub disk_read_bytes_per_s: Rate,
    pub disk_write_bytes_per_s: Rate,
    /// Watts, from the kernel's per-process energy counter; `None` where it has none.
    pub energy_w: Option<Rate>,
}

impl Usage {
    fn empty() -> Usage {
        Usage {
            kind: None,
            id: String::new(),
            pid: None,
            pids: 0,
            cpu_pct: Rate::default(),
            resident_bytes: 0,
            footprint_bytes: 0,
            disk_read_bytes_per_s: Rate::default(),
            disk_write_bytes_per_s: Rate::default(),
            energy_w: None,
        }
    }
}

/// The host's totals and the devices' and simee's share of its busy CPU.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Host {
    pub cpus: usize,
    /// Busy CPU, percent of one core (`cpus * 100` is every core busy).
    pub cpu_pct: Rate,
    pub memory_total_bytes: Option<u64>,
    pub devices: usize,
    pub devices_cpu_pct: Rate,
    pub devices_resident_bytes: u64,
    pub devices_footprint_bytes: u64,
    /// The devices' CPU as a percent of the host's busy CPU.
    pub devices_share_pct: Rate,
    pub simee_share_pct: Rate,
}

/// What `devices_usage` reports.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    /// Whether this host's processes can be attributed (macOS: libproc).
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub samples: u64,
    /// The time the 1 s and 1 min windows actually span.
    pub window_s: Rate,
    /// Costliest first: by 1-minute CPU, then footprint.
    pub devices: Vec<Usage>,
    pub simee: Usage,
    pub host: Host,
}

/// Whether [`Reader`] can read this host's processes.
pub const AVAILABLE: bool = cfg!(target_os = "macos");
const UNAVAILABLE: &str = "per-device attribution reads libproc, which only macOS has";

/// Reads observations from the host: keeps each device process's arguments, by pid and start
/// time, so a process's are read once.
#[derive(Default)]
pub struct Reader {
    #[cfg(target_os = "macos")]
    argv: HashMap<i32, (u64, Vec<String>, String)>,
}

impl Reader {
    /// One pass over the host's processes; `None` where they can't be read.
    #[cfg(target_os = "macos")]
    pub fn observe(&mut self) -> Option<Observation> {
        use sim_coresim::procs;
        let at_ns = procs::now_ns();
        let counters_of = |u: procs::Usage| Counters {
            start_ns: u.start_ns,
            cpu_ns: u.cpu_ns,
            child_cpu_ns: u.reaped_children_cpu_ns,
            resident_bytes: u.resident_bytes,
            footprint_bytes: u.footprint_bytes,
            disk_read_bytes: u.disk_read_bytes,
            disk_written_bytes: u.disk_written_bytes,
            energy_nj: u.energy_nj,
        };
        let table = procs::list();
        let mut read: HashMap<i32, Counters> = HashMap::new();
        let mut procs_out = Vec::with_capacity(table.len());
        for row in &table {
            let name = row.name();
            let (mut argv, mut exe) = (Vec::new(), String::new());
            if wants_argv(name)
                && let Some(c) = procs::usage(row.pid).map(counters_of)
            {
                (argv, exe) = match self.argv.get(&row.pid) {
                    Some((start, a, e)) if *start == c.start_ns => (a.clone(), e.clone()),
                    _ => {
                        let b = procs::procargs(row.pid);
                        let a = b.as_deref().map(argv_from_procargs).unwrap_or_default();
                        let e = b.as_deref().map(exe_from_procargs).unwrap_or_default();
                        self.argv.insert(row.pid, (c.start_ns, a.clone(), e.clone()));
                        (a, e)
                    }
                };
                read.insert(row.pid, c);
            }
            procs_out.push(Proc { pid: row.pid, ppid: row.ppid, name: name.to_owned(), argv, exe });
        }
        self.argv.retain(|pid, _| read.contains_key(pid));
        let attribution = attribute(&procs_out);
        let mut counters = HashMap::new();
        for &pid in attribution.devices.iter().flat_map(|g| g.pids.iter()).chain(attribution.simee.iter()) {
            if let Some(c) = read.get(&pid).copied().or_else(|| procs::usage(pid).map(counters_of)) {
                counters.insert(pid, c);
            }
        }
        Some(Observation { at_ns, attribution, counters, host_ticks: sim_coresim::host::cpu_ticks() })
    }

    /// One pass over the host's processes; `None` where they can't be read.
    #[cfg(not(target_os = "macos"))]
    pub fn observe(&mut self) -> Option<Observation> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, name: &str, argv: &[&str]) -> Proc {
        Proc { pid, ppid, name: name.into(), argv: argv.iter().map(|s| s.to_string()).collect(), exe: String::new() }
    }

    const UDID: &str = "8B529975-BB5F-4ED5-8309-0FF04764BAE6";

    fn bootstrap(udid: &str) -> String {
        format!("/Users/me/Library/Developer/CoreSimulator/Devices/{udid}/data/var/run/launchd_bootstrap.plist")
    }

    #[test]
    fn an_ios_device_is_its_launchd_sim_tree_named_by_udid() {
        // The runtime's cryptex path carries a UUID too; only the one before `/data` is the device.
        let exe =
            "/Library/Developer/CoreSimulator/Cryptex/Images/bundle/SimRuntimeBundle-0F3A5B6C-1D2E-4F50-8A9B-ABCDEF012345/launchd_sim";
        let other = "11111111-2222-3333-4444-555555555555";
        let table = [
            p(1, 0, "launchd", &[]),
            p(100, 1, "launchd_sim", &[exe, &bootstrap(UDID)]),
            p(101, 100, "SpringBoard", &[]),
            p(102, 100, "backboardd", &[]),
            p(110, 101, "MobileSafari", &[]),
            p(200, 1, "launchd_sim", &["launchd_sim", &bootstrap(other)]),
            p(201, 200, "SpringBoard", &[]),
            p(300, 1, "launchd_sim", &["launchd_sim"]),
            p(400, 1, "zsh", &[]),
        ];
        let a = attribute(&table);
        let ids: Vec<(&str, Vec<i32>)> = a.devices.iter().map(|g| (g.id.as_str(), g.pids.clone())).collect();
        assert_eq!(ids, [(UDID, vec![100, 101, 102, 110]), (other, vec![200, 201]), ("launchd_sim pid 300", vec![300])]);
        assert!(a.devices.iter().all(|g| g.kind == Kind::Simulator && g.kind.platform() == "ios"));
        assert!(a.simee.is_empty(), "{a:?}");
    }

    #[test]
    fn simee_emu_is_a_device_with_its_children_and_simee_owns_the_rest() {
        let table = [
            p(1, 0, "launchd", &[]),
            p(500, 1, "simee", &["simee", "serve"]),
            p(510, 500, "emu", &["/x/target/release/emu", "-avd", "lane-a-1", "-no-boot-anim"]),
            p(511, 510, "qemu-system-aar", &["qemu-system-aarch64", "-machine", "virt"]),
            p(512, 510, "adb", &["adb", "wait-for-device"]),
            p(520, 500, "simaudio", &[]),
            p(530, 1, "emu", &["emu", "@pixel"]),
            p(540, 1, "emu", &["emu", "-list-avds"]),
            p(600, 1, "simee-desktop", &[]),
            p(601, 600, "simee", &["simee", "serve"]),
        ];
        let a = attribute(&table);
        let got: Vec<(Kind, &str, i32, Vec<i32>)> = a.devices.iter().map(|g| (g.kind, g.id.as_str(), g.root, g.pids.clone())).collect();
        assert_eq!(got, [(Kind::Emu, "lane-a-1", 510, vec![510, 511, 512]), (Kind::Emu, "pixel", 530, vec![530])]);
        assert_eq!(a.simee, [500, 520, 540, 600, 601], "the server, its helper, an emu that runs no AVD, the desktop");
    }

    #[test]
    fn stock_emulator_families_follow_spectras_rules() {
        let qemu = "/Users/me/Library/Android/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless";
        let table = [
            p(1, 0, "launchd", &[]),
            // qemu started straight with -avd (its launcher exited): its own tree.
            p(700, 1, "qemu-system-aar", &[qemu, "-avd", "acceptance-pixel-9", "-port", "5556"]),
            p(701, 700, "crashpad_handle", &[]),
            // Under a launcher that names the AVD; the launcher's other child comes along.
            p(800, 1, "emulator", &["emulator", "-avd=pixel_fold"]),
            p(801, 800, "qemu-system-aar", &["qemu-system-aarch64", "-netdelay", "none"]),
            p(802, 800, "crashpad_handle", &[]),
            // From the SDK's qemu directory with no AVD named: a device by pid.
            p(900, 1, "qemu-system-aar", &[qemu, "-kernel", "k"]),
            // A bare qemu (colima, lima): not a device.
            p(950, 1, "qemu-system-x86", &["/opt/homebrew/bin/qemu-system-x86_64", "-m", "2048"]),
            // A launcher with no qemu yet.
            p(980, 1, "emulator", &["emulator", "@wear"]),
        ];
        let a = attribute(&table);
        let got: Vec<(&str, i32, Vec<i32>)> = a.devices.iter().map(|g| (g.id.as_str(), g.root, g.pids.clone())).collect();
        assert_eq!(
            got,
            [
                ("acceptance-pixel-9", 700, vec![700, 701]),
                ("pixel_fold", 801, vec![801, 800, 802]),
                ("qemu-system-aar pid 900", 900, vec![900]),
                ("wear", 980, vec![980]),
            ]
        );
        assert!(a.devices.iter().all(|g| g.kind == Kind::Emulator && g.kind.platform() == "android"));
        assert!(!a.devices.iter().any(|g| g.pids.contains(&950)));
    }

    #[test]
    fn avd_names_udids_and_procargs_parse() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(avd_name(&v(&["emulator", "-avd", "a"])).as_deref(), Some("a"));
        assert_eq!(avd_name(&v(&["emulator", "-avd=b"])).as_deref(), Some("b"));
        assert_eq!(avd_name(&v(&["emulator", "-no-window", "@c"])).as_deref(), Some("c"));
        assert_eq!(avd_name(&v(&["@argv0-is-not-an-avd"])), None);
        assert_eq!(avd_name(&v(&["emulator", "-avd"])), None);
        assert_eq!(udid_in(&v(&["launchd_sim", &bootstrap(UDID)])).as_deref(), Some(UDID));
        assert_eq!(udid_in(&v(&["x", "/a/8B529975-BB5F-4ED5-8309-0FF04764BAE6x/data"])), None);
        let mut rec = 3i32.to_ne_bytes().to_vec();
        rec.extend_from_slice(b"/bin/emu\0\0\0\0emu\0\0-avd\0x\0HOME=/Users/me\0");
        assert_eq!(argv_from_procargs(&rec), ["emu", "", "-avd"], "argc strings, an empty one kept, no environment");
        assert!(argv_from_procargs(b"\x01").is_empty());
    }

    fn c(start_ns: u64, cpu_ns: u64) -> Counters {
        Counters { start_ns, cpu_ns, resident_bytes: 10, footprint_bytes: 20, ..Counters::default() }
    }

    fn obs(at_s: u64, devices: Vec<Group>, counters: &[(i32, Counters)]) -> Observation {
        Observation {
            at_ns: at_s * S1,
            attribution: Attribution { devices, simee: vec![] },
            counters: counters.iter().copied().collect(),
            host_ticks: None,
        }
    }

    fn sim(pids: &[i32]) -> Group {
        Group { kind: Kind::Simulator, id: UDID.into(), root: pids[0], pids: pids.to_vec() }
    }

    #[test]
    fn windowed_cpu_is_the_deltas_over_the_time_they_span() {
        let mut t = Tracker::new(8);
        // The first sample is a baseline: nothing to rate yet.
        t.update(obs(10, vec![sim(&[1, 2])], &[(1, c(1, 50 * S1)), (2, c(2, 7 * S1))]));
        let r = t.report();
        assert_eq!((r.devices[0].cpu_pct, r.samples, r.window_s.s1), (Rate::default(), 1, None));
        // 1 s later: pid 1 used half a core, pid 2 a quarter.
        t.update(obs(11, vec![sim(&[1, 2])], &[(1, c(1, 50 * S1 + S1 / 2)), (2, c(2, 7 * S1 + S1 / 4))]));
        assert_eq!(t.report().devices[0].cpu_pct, Rate { s1: Some(75.0), m1: Some(75.0) });
        // 59 more seconds idle, then one busy second: the second is 200 %, the minute (which no
        // longer holds the 75 % second) 200/60.
        let mut cpu1 = 50 * S1 + S1 / 2;
        for at in 12..=71 {
            if at == 71 {
                cpu1 += 2 * S1;
            }
            t.update(obs(at, vec![sim(&[1, 2])], &[(1, c(1, cpu1)), (2, c(2, 7 * S1 + S1 / 4))]));
        }
        let r = t.report();
        let cpu = r.devices[0].cpu_pct;
        assert_eq!(cpu.s1, Some(200.0));
        assert!(cpu.m1.is_some_and(|m| (m - 200.0 / 60.0).abs() < 1e-9), "{cpu:?}");
        assert_eq!(r.window_s, Rate { s1: Some(1.0), m1: Some(60.0) });
        assert_eq!((r.devices[0].pids, r.devices[0].resident_bytes, r.devices[0].footprint_bytes), (2, 20, 40));
    }

    #[test]
    fn a_reused_pid_is_a_new_process_and_a_new_one_counts_whole() {
        let mut t = Tracker::new(8);
        t.update(obs(10, vec![sim(&[1, 5])], &[(1, c(1, 0)), (5, c(3 * S1, 900 * S1))]));
        // pid 5 exited and was reused by a process that started at 10.5 s and has used 0.3 s;
        // pid 6 started at 10.2 s and used 0.1 s.
        t.update(obs(11, vec![sim(&[1, 5, 6])], &[(1, c(1, 0)), (5, c(10 * S1 + S1 / 2, 3 * S1 / 10)), (6, c(10 * S1 + S1 / 5, S1 / 10))]));
        let cpu = t.report().devices[0].cpu_pct.s1.expect("rate");
        assert!((cpu - 40.0).abs() < 1e-9, "{cpu}: the old pid 5's 900 s is never subtracted, the new ones count whole");
        // A process first seen now that started before the last sample is a baseline (0), not its
        // lifetime's CPU.
        t.update(obs(
            12,
            vec![sim(&[1, 5, 6, 7])],
            &[(1, c(1, 0)), (5, c(10 * S1 + S1 / 2, 3 * S1 / 10)), (6, c(10 * S1 + S1 / 5, S1 / 10)), (7, c(2 * S1, 500 * S1))],
        ));
        assert_eq!(t.report().devices[0].cpu_pct.s1, Some(0.0));
    }

    #[test]
    fn rows_sort_by_cost_and_the_host_shares_add_up() {
        let mut t = Tracker::new(4);
        let stock = |pid| Group { kind: Kind::Emulator, id: "pixel".into(), root: pid, pids: vec![pid] };
        let ticks = |busy: u32, idle: u32| Some([busy, 0, idle, 0]);
        let mut o = obs(0, vec![sim(&[1]), stock(2)], &[(1, c(1, 0)), (2, c(1, 0)), (3, c(1, 0))]);
        o.attribution.simee = vec![3];
        o.host_ticks = ticks(u32::MAX - 50, 0); // wraps before the next sample
        t.update(o);
        let mut o = obs(1, vec![sim(&[1]), stock(2)], &[(1, c(1, S1 / 10)), (2, c(1, 2 * S1)), (3, c(1, S1 / 5))]);
        o.attribution.simee = vec![3];
        // 400 ticks over 4 CPUs: 300 busy, so 3 cores.
        o.host_ticks = ticks(249, 100);
        o.counters.get_mut(&2).expect("stock").energy_nj = Some(0);
        t.update(o);
        let r = t.report();
        let order: Vec<(&str, Option<f64>)> = r.devices.iter().map(|d| (d.id.as_str(), d.cpu_pct.s1)).collect();
        assert_eq!(order, [("pixel", Some(200.0)), (UDID, Some(10.0))]);
        assert_eq!((r.devices[0].pid, r.devices[1].kind), (Some(2), Some(Kind::Simulator)));
        assert_eq!(r.simee.cpu_pct.s1, Some(20.0));
        assert_eq!(r.host.cpu_pct.s1, Some(300.0));
        assert_eq!(r.host.devices_cpu_pct.s1, Some(210.0));
        let near = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 1e-9);
        assert!(near(r.host.devices_share_pct.s1, 70.0) && near(r.host.simee_share_pct.s1, 20.0 / 3.0), "{:?}", r.host);
        assert_eq!((r.devices[0].energy_w.map(|e| e.s1), r.devices[1].energy_w), (Some(Some(0.0)), None));
        // A device that stopped leaves the report.
        t.update(obs(2, vec![sim(&[1])], &[(1, c(1, S1 / 10))]));
        assert_eq!(t.report().devices.len(), 1);
    }

    #[test]
    fn instances_of_one_avd_are_separate_rows_and_all_count_in_the_totals() {
        let mut t = Tracker::new(8);
        let inst = |pid| Group { kind: Kind::Emu, id: "pixel_9".into(), root: pid, pids: vec![pid] };
        let at = |s: u64, a: u64, b: u64| obs(s, vec![inst(10), inst(20)], &[(10, c(1, a)), (20, c(2, b))]);
        t.update(at(0, 0, 0));
        t.update(at(1, S1, S1 / 2));
        let r = t.report();
        let rows: Vec<(&str, Option<i32>, Option<f64>)> = r.devices.iter().map(|d| (d.id.as_str(), d.pid, d.cpu_pct.s1)).collect();
        assert_eq!(rows, [("pixel_9 #1", Some(10), Some(100.0)), ("pixel_9 #2", Some(20), Some(50.0))]);
        assert_eq!((r.host.devices, r.host.devices_cpu_pct.s1), (2, Some(150.0)));
        assert_eq!(r.host.devices_footprint_bytes, 40);
        // The first instance exits: the survivor keeps its own history and its plain name.
        t.update(obs(2, vec![inst(20)], &[(20, c(2, S1 / 2 + S1))]));
        let r = t.report();
        assert_eq!((r.devices.len(), r.devices[0].id.as_str(), r.devices[0].cpu_pct.s1), (1, "pixel_9", Some(100.0)));
        assert_eq!(r.devices[0].cpu_pct.m1, Some(75.0));
    }

    #[test]
    fn a_stock_qemu_is_found_by_its_executable_path_alone() {
        let mut bare = p(700, 1, "qemu-system-aar", &["qemu-system-aarch64", "-kernel", "k"]);
        assert!(attribute(&[p(1, 0, "launchd", &[]), bare.clone()]).devices.is_empty(), "a bare qemu is not a device");
        bare.exe = "/Users/me/Library/Android/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless".into();
        let a = attribute(&[p(1, 0, "launchd", &[]), bare]);
        let got: Vec<(&str, i32)> = a.devices.iter().map(|g| (g.id.as_str(), g.root)).collect();
        assert_eq!(got, [("qemu-system-aar pid 700", 700)]);
        let mut rec = 2i32.to_ne_bytes().to_vec();
        rec.extend_from_slice(b"/sdk/emulator/qemu/x/qemu-system-aarch64\0\0\0qemu-system-aarch64\0-m\0");
        assert_eq!(exe_from_procargs(&rec), "/sdk/emulator/qemu/x/qemu-system-aarch64");
        assert_eq!(exe_from_procargs(b"\x01"), "");
    }

    #[test]
    fn cpu_of_children_that_exited_between_samples_is_recovered() {
        let mut t = Tracker::new(8);
        let root = |cpu, child| Counters { child_cpu_ns: child, ..c(1, cpu) };
        t.update(obs(0, vec![sim(&[1, 2])], &[(1, root(0, 0)), (2, c(2, 3 * S1))]));
        // Pid 2 (3 s seen) exited with 5 s in all, and a short-lived pid 3 (never seen) used 1 s:
        // the root reaped 6 s. The root itself used 0.5 s. The 3 s already seen are not counted twice.
        t.update(obs(1, vec![sim(&[1])], &[(1, root(S1 / 2, 6 * S1))]));
        assert_eq!(t.report().devices[0].cpu_pct.s1, Some(350.0));
        // Nothing exited: the same counters add nothing.
        t.update(obs(2, vec![sim(&[1])], &[(1, root(S1 / 2, 6 * S1))]));
        assert_eq!(t.report().devices[0].cpu_pct.s1, Some(0.0));
        assert_eq!((recovered_child_cpu(5, 9), recovered_child_cpu(9, 5)), (0, 4));
    }

    #[test]
    fn energy_and_disk_rates() {
        let mut t = Tracker::new(1);
        let with =
            |cpu, read, written, nj| Counters { disk_read_bytes: read, disk_written_bytes: written, energy_nj: Some(nj), ..c(1, cpu) };
        t.update(obs(0, vec![sim(&[1])], &[(1, with(0, 1000, 0, 5_000_000_000))]));
        t.update(obs(2, vec![sim(&[1])], &[(1, with(0, 5000, 2000, 8_000_000_000))]));
        let d = &t.report().devices[0];
        assert_eq!(d.disk_read_bytes_per_s.s1, Some(2000.0), "the closest window to 1 s is the one 2 s step");
        assert_eq!(d.disk_write_bytes_per_s.m1, Some(1000.0));
        assert_eq!(d.energy_w.and_then(|e| e.s1), Some(1.5));
    }

    /// A fake device listing, two observations 1 s apart: the device is listed with its rates.
    #[test]
    fn fake_devices_are_listed() {
        let mut t = Tracker::new(4);
        t.update(obs(0, vec![sim(&[1, 2])], &[(1, c(1, 0)), (2, c(2, 0))]));
        t.update(obs(1, vec![sim(&[1, 2])], &[(1, c(1, S1 / 2)), (2, c(2, S1 / 4))]));
        let r = t.report();
        assert_eq!((r.samples, r.devices.len(), r.host.devices), (2, 1, 1), "{r:?}");
        let d = &r.devices[0];
        assert_eq!((d.kind, d.id.as_str(), d.pid, d.pids), (Some(Kind::Simulator), UDID, Some(1), 2), "{d:?}");
        assert_eq!((d.cpu_pct.s1, d.footprint_bytes), (Some(75.0), 40), "{d:?}");
    }

    /// The live host: whatever is running is listed (nothing booted or touched). Load-sensitive,
    /// so it only runs on request.
    /// `cargo test -p simee-host live_devices -- --ignored --nocapture`
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "reads the live host; run with --ignored"]
    fn live_devices_are_listed() {
        let mut reader = Reader::default();
        let mut t = Tracker::new(std::thread::available_parallelism().map_or(1, |n| n.get()));
        t.update(reader.observe().expect("macOS reads libproc"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        t.update(reader.observe().expect("observe"));
        let r = t.report();
        // In a macOS VM (GitHub's runners) the host's 100 Hz tick counters may not move within
        // 300 ms, so host CPU can be unknown there; on real hardware it must be measured.
        let in_vm = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.hv_vmm_present"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "1");
        assert!(r.available && r.samples == 2 && (r.host.cpu_pct.s1.is_some() || in_vm), "{r:?}");
        for d in &r.devices {
            assert!(d.pids >= 1 && d.footprint_bytes > 0 && d.cpu_pct.s1.is_some(), "{d:?}");
            println!(
                "{:?} {} pid {:?}: {} procs, {:.1}% cpu, {} MiB footprint, energy {:?}",
                d.kind,
                d.id,
                d.pid,
                d.pids,
                d.cpu_pct.s1.unwrap_or(0.0),
                d.footprint_bytes >> 20,
                d.energy_w.and_then(|e| e.s1)
            );
        }
        println!("host {:?}; simee {} procs", r.host, r.simee.pids);
    }

    /// `cargo test -p simee-host --release observe_cost -- --ignored --nocapture`
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "measures the per-observation CPU cost"]
    fn observe_cost() {
        let mut reader = Reader::default();
        reader.observe();
        let n = 50;
        let t = std::time::Instant::now();
        let mut pids = 0;
        for _ in 0..n {
            let o = std::hint::black_box(reader.observe().expect("observe"));
            pids = o.counters.len();
        }
        let per = t.elapsed() / n;
        println!("one observation ({pids} attributed processes): {per:?} = {:.3}% of a core at 1 Hz", per.as_secs_f64() * 100.0);
    }
}
