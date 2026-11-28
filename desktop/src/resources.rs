//! The resource panel (#276): what the host has left and what each running device costs, in the
//! inspector. Host gauges (busy CPU, load per core, memory pressure, swap, disk, thermal state and
//! the admission verdict) come from the API's `host`; the device list, with a sparkline of the
//! last minute per device, from `devices_usage`; a device's running stream from `streams`. Each
//! row has quick actions: show the device in the window, stop its stream, shut it down.
//!
//! ⌘⌥R, View > Resources or the gauge button in the inspector's top-right corner shows and hides
//! it in place of the platform panel. Its visibility and sort persist in [`crate::settings`].
//!
//! Idle cost is near zero: the panel samples at most once every [`INTERVAL`] and only while it
//! shows (the inspector visible, the window on screen); hidden, it asks for no sample and no
//! wake. The API reads the host sampler the window's API server already runs (`sim_host::global`),
//! on a worker thread of its own, so the UI never waits. A sample that changes nothing on screen
//! (values are rounded to what the panel shows) draws no frame.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rune::tok::{self, Theme, radius, type_};
use rune::{Color, El, ScrollSpec, Shape, Tx, canvas, col, el, row, text};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sim_api::Session;

use crate::panes::{Cause, Side, Target};
use crate::{Action, State, icons};

/// Seconds between samples while the panel shows.
pub const INTERVAL: f64 = 1.0;

/// Samples a sparkline keeps: the last minute at [`INTERVAL`].
pub const HISTORY: usize = 60;

/// A gap this long between two samples (the panel was hidden) starts the sparklines afresh, so
/// they never join two minutes that weren't next to each other.
const GAP: f64 = 3.0 * INTERVAL;

/// What the device list is sorted by, costliest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Cpu,
    Memory,
}

/// The panel's saved settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub visible: bool,
    pub sort: Sort,
}

// ----- data -------------------------------------------------------------------------------------

/// The host, rounded to what the gauges show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Host {
    pub cpus: usize,
    /// Busy CPU over the last second, percent of one core (`cpus * 100` is every core busy).
    pub cpu_pct: Option<f64>,
    pub load_one: Option<f64>,
    /// `normal`, `warning` or `critical`.
    pub memory_level: Option<String>,
    pub memory_free_pct: Option<f64>,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disk_free: Option<u64>,
    pub disk_total: Option<u64>,
    /// `nominal`, `fair`, `serious` or `critical`.
    pub thermal: Option<String>,
    /// `admit`, `hold` or `backoff`.
    pub verdict: Option<String>,
}

/// One running device, rounded to what its row shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// A simulator's UDID or an AVD's name (what the API and the window's list call it).
    pub id: String,
    pub name: String,
    /// `ios` or `android`.
    pub platform: String,
    /// CPU over the last second, percent of one core.
    pub cpu_pct: Option<f64>,
    /// Physical footprint (resident where the host reports none).
    pub memory: u64,
    pub read_per_s: Option<f64>,
    pub write_per_s: Option<f64>,
    pub energy_w: Option<f64>,
    /// The device's running stream (`stream_id`), if one runs in this process.
    pub stream: Option<String>,
}

/// One sample: the host and the running devices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub host: Host,
    pub rows: Vec<Row>,
    /// Why per-device usage is unavailable on this host.
    pub unavailable: Option<String>,
}

fn round(v: Option<f64>, step: f64) -> Option<f64> {
    v.filter(|v| v.is_finite()).map(|v| (v / step).round() * step)
}

/// Bytes rounded to the MiB.
fn mib(v: u64) -> u64 {
    (v + (1 << 19)) >> 20 << 20
}

fn opt_u64(v: &Value) -> Option<u64> {
    v.as_u64()
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

/// A sample from the API's `host`, `devices_usage` and `streams` answers.
pub fn reading(host: &Value, usage: &Value, streams: &Value) -> Reading {
    let snap = &host["snapshot"];
    let mem = &snap["memory"];
    let cpus = snap["cpus"].as_u64().or_else(|| usage["host"]["cpus"].as_u64()).unwrap_or(0) as usize;
    let h = Host {
        cpus,
        cpu_pct: round(usage["host"]["cpu_pct"]["1s"].as_f64(), 1.0),
        load_one: round(snap["load"]["one"].as_f64(), 0.01),
        memory_level: opt_str(&mem["level"]),
        memory_free_pct: round(mem["free_percent"].as_f64(), 1.0),
        swap_used: mem["swap_used_bytes"].as_u64().map_or(0, mib),
        swap_total: mem["swap_total_bytes"].as_u64().map_or(0, mib),
        disk_free: opt_u64(&snap["disk"]["free_bytes"]).map(mib),
        disk_total: opt_u64(&snap["disk"]["total_bytes"]).map(mib),
        thermal: opt_str(&host["thermal_state"]),
        verdict: opt_str(&host["verdict"]["verdict"]),
    };
    let live: Vec<(&str, &str)> = streams
        .as_array()
        .map(|a| {
            a.iter().filter(|s| s["state"] != "ended").filter_map(|s| Some((s["device"].as_str()?, s["stream_id"].as_str()?))).collect()
        })
        .unwrap_or_default();
    let rows = usage["devices"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let id = d["id"].as_str()?.to_owned();
                    let footprint = d["footprint_bytes"].as_u64().unwrap_or(0);
                    let memory = if footprint > 0 { footprint } else { d["resident_bytes"].as_u64().unwrap_or(0) };
                    let stream = live.iter().find(|(dev, _)| dev.eq_ignore_ascii_case(&id)).map(|(_, s)| (*s).to_owned());
                    Some(Row {
                        name: d["name"].as_str().unwrap_or(&id).to_owned(),
                        platform: d["platform"].as_str().unwrap_or("").to_owned(),
                        cpu_pct: round(d["cpu_pct"]["1s"].as_f64(), 0.1),
                        memory: mib(memory),
                        read_per_s: round(d["disk_read_bytes_per_s"]["1s"].as_f64(), 1024.0),
                        write_per_s: round(d["disk_write_bytes_per_s"]["1s"].as_f64(), 1024.0),
                        energy_w: round(d["energy_w"]["1s"].as_f64(), 0.1),
                        stream,
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let unavailable = (usage["available"] == false).then(|| usage["reason"].as_str().unwrap_or("unavailable").to_owned());
    Reading { host: h, rows, unavailable }
}

/// `rows`, costliest first by `by`; a device with no rate yet goes last, ties by name.
pub fn sorted(rows: &[Row], by: Sort) -> Vec<&Row> {
    let mut out: Vec<&Row> = rows.iter().collect();
    out.sort_by(|a, b| {
        let key = match by {
            Sort::Cpu => b.cpu_pct.unwrap_or(-1.0).total_cmp(&a.cpu_pct.unwrap_or(-1.0)).then(b.memory.cmp(&a.memory)),
            Sort::Memory => b.memory.cmp(&a.memory).then(b.cpu_pct.unwrap_or(-1.0).total_cmp(&a.cpu_pct.unwrap_or(-1.0))),
        };
        key.then_with(|| a.name.cmp(&b.name)).then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// The last [`HISTORY`] values of one series, in a fixed ring: no allocation per sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Spark {
    buf: [f32; HISTORY],
    len: usize,
    /// Where the next value goes.
    head: usize,
}

impl Default for Spark {
    fn default() -> Self {
        Spark { buf: [0.0; HISTORY], len: 0, head: 0 }
    }
}

impl Spark {
    /// Adds `v`, dropping the oldest once full. Returns whether the line changed: a full ring of
    /// the same value stays the same line.
    pub fn push(&mut self, v: f32) -> bool {
        let same = self.len == HISTORY && self.buf.iter().all(|&x| x.to_bits() == v.to_bits());
        self.buf[self.head] = v;
        self.head = (self.head + 1) % HISTORY;
        self.len = (self.len + 1).min(HISTORY);
        !same
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The values, oldest first.
    pub fn values(&self) -> impl Iterator<Item = f32> + '_ {
        let start = (self.head + HISTORY - self.len) % HISTORY;
        (0..self.len).map(move |i| self.buf[(start + i) % HISTORY])
    }
}

/// A device's two sparklines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    pub cpu: Spark,
    /// MiB.
    pub memory: Spark,
}

/// When to sample: at most once every [`INTERVAL`], one at a time, only while showing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pacer {
    asked: Option<f64>,
    in_flight: bool,
}

impl Pacer {
    pub fn due(&self, now: f64, showing: bool) -> bool {
        showing && !self.in_flight && self.asked.is_none_or(|a| now - a >= INTERVAL)
    }

    pub fn ask(&mut self, now: f64) {
        self.asked = Some(now);
        self.in_flight = true;
    }

    pub fn landed(&mut self) {
        self.in_flight = false;
    }

    /// The next sample is due at once (after a quick action, so the gauges follow it).
    pub fn hurry(&mut self) {
        self.asked = None;
    }

    #[cfg(test)]
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Seconds until the next sample is due, for a wake; `None` while hidden or waiting on one
    /// (the worker wakes the window when it lands).
    pub fn wake_in(&self, now: f64, showing: bool) -> Option<f64> {
        if !showing || self.in_flight {
            return None;
        }
        Some(self.asked.map_or(0.0, |a| (a + INTERVAL - now).max(0.0)))
    }
}

// ----- the worker -------------------------------------------------------------------------------

/// Work for the panel's API thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Sample,
    /// Shut down a device the window doesn't show (the shown one goes through [`Action::Shutdown`]).
    Shutdown {
        device: String,
        platform: String,
        name: String,
    },
    StopStream {
        stream: String,
        name: String,
    },
}

impl Job {
    /// The API calls the job makes, in order.
    pub fn calls(&self) -> Vec<(&'static str, Value)> {
        match self {
            Job::Sample => vec![("host", json!({})), ("devices_usage", json!({})), ("streams", json!({}))],
            Job::Shutdown { device, platform, .. } => vec![("shutdown", json!({"device": device, "platform": platform}))],
            Job::StopStream { stream, .. } => vec![("stream_stop", json!({"stream_id": stream}))],
        }
    }
}

/// What a finished job hands back.
#[derive(Debug)]
enum Outcome {
    Sample(Result<Reading, String>),
    /// A quick action's status line, whether it worked, and the device it shut down.
    Done(String, bool, Option<String>),
}

/// The API, as the panel calls it (a stand-in in tests).
pub type Call = fn(&mut Session, &str, &Value) -> Result<Value, String>;

fn run(call: Call, session: &mut Session, job: &Job) -> Outcome {
    let mut answers = job.calls().into_iter().map(|(method, params)| call(session, method, &params));
    match job {
        Job::Sample => {
            let host = answers.next().unwrap_or_else(|| Err("no answer".into()));
            let usage = answers.next().unwrap_or_else(|| Err("no answer".into()));
            let streams = answers.next().and_then(Result::ok).unwrap_or(Value::Null);
            Outcome::Sample(host.map(|h| {
                let mut r = reading(&h, usage.as_ref().unwrap_or(&Value::Null), &streams);
                if let Err(e) = usage {
                    r.unavailable = Some(e);
                }
                r
            }))
        }
        Job::Shutdown { name, device, .. } => match answers.next() {
            Some(Ok(_)) => Outcome::Done(format!("{name}: shut down"), true, Some(device.clone())),
            Some(Err(e)) => Outcome::Done(format!("shut down {name}: {e}"), false, None),
            None => Outcome::Done(format!("shut down {name}: no answer"), false, None),
        },
        Job::StopStream { name, .. } => match answers.next() {
            Some(Ok(_)) => Outcome::Done(format!("{name}: stream stopped"), true, None),
            Some(Err(e)) => Outcome::Done(format!("stop {name}'s stream: {e}"), false, None),
            None => Outcome::Done(format!("stop {name}'s stream: no answer"), false, None),
        },
    }
}

struct Worker {
    jobs: mpsc::Sender<Job>,
    results: Arc<Mutex<Vec<Outcome>>>,
}

impl Worker {
    fn spawn(call: Call, waker: rune::Waker) -> Option<Worker> {
        let (tx, rx) = mpsc::channel::<Job>();
        let results: Arc<Mutex<Vec<Outcome>>> = Arc::default();
        let out = results.clone();
        let spawned = std::thread::Builder::new().name("simee-resources".into()).spawn(move || {
            let mut session = Session::default();
            for job in rx {
                let outcome = run(call, &mut session, &job);
                if let Ok(mut r) = out.lock() {
                    r.push(outcome);
                }
                waker();
            }
        });
        spawned.ok().map(|_| Worker { jobs: tx, results })
    }
}

/// What one [`Resources::tick`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Tick {
    /// Something on screen changed.
    pub changed: bool,
    /// Status lines from quick actions.
    pub status: Vec<String>,
    /// A quick action worked: the device list should be scanned again.
    pub rescan: bool,
    /// Devices the API shut down.
    pub stopped: Vec<String>,
    /// Seconds until the next sample is due.
    pub wake: Option<f64>,
}

/// The panel's state: the last sample, the sparklines, and its line to the API.
pub struct Resources {
    pub reading: Option<Reading>,
    pub history: HashMap<String, History>,
    /// Why the last sample failed.
    pub error: Option<String>,
    /// Quick actions waiting for `poll`.
    pub queued: Vec<Job>,
    pacer: Pacer,
    landed_at: Option<f64>,
    call: Call,
    /// Set by `setup` in a live window: headless scenes and tests never start the thread.
    waker: Option<rune::Waker>,
    worker: Option<Worker>,
}

impl Default for Resources {
    fn default() -> Self {
        Self::with_call(sim_api::call_method)
    }
}

impl Resources {
    pub fn with_call(call: Call) -> Self {
        Resources {
            reading: None,
            history: HashMap::new(),
            error: None,
            queued: Vec::new(),
            pacer: Pacer::default(),
            landed_at: None,
            call,
            waker: None,
            worker: None,
        }
    }

    /// Lets the panel reach the API; its thread starts at the first sample, so a panel never
    /// shown never starts one.
    pub fn connect(&mut self, waker: rune::Waker) {
        self.waker = Some(waker);
    }

    fn send(&mut self, job: Job) -> bool {
        if self.worker.is_none()
            && let Some(waker) = self.waker.clone()
        {
            self.worker = Worker::spawn(self.call, waker);
        }
        self.worker.as_ref().is_some_and(|w| w.jobs.send(job).is_ok())
    }

    /// Applies a sample that landed at `now`. Returns whether anything shown changed.
    pub fn apply(&mut self, now: f64, r: Reading) -> bool {
        let mut changed = self.error.take().is_some();
        if self.landed_at.is_some_and(|at| now - at > GAP) {
            self.history.clear();
        }
        self.landed_at = Some(now);
        let before = self.history.len();
        self.history.retain(|id, _| r.rows.iter().any(|row| &row.id == id));
        changed |= self.history.len() != before;
        for row in &r.rows {
            let h = self.history.entry(row.id.clone()).or_default();
            changed |= h.cpu.push(row.cpu_pct.unwrap_or(0.0) as f32);
            changed |= h.memory.push((row.memory >> 20) as f32);
        }
        changed |= self.reading.as_ref() != Some(&r);
        self.reading = Some(r);
        changed
    }

    /// One poll at `now`: applies what the worker finished, sends queued actions, and asks for a
    /// sample when one is due and the panel is `showing`.
    pub fn tick(&mut self, now: f64, showing: bool) -> Tick {
        let mut tick = Tick::default();
        let done: Vec<Outcome> = match &self.worker {
            Some(w) => w.results.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default(),
            None => Vec::new(),
        };
        for outcome in done {
            match outcome {
                Outcome::Sample(Ok(r)) => {
                    self.pacer.landed();
                    tick.changed |= self.apply(now, r);
                }
                Outcome::Sample(Err(e)) => {
                    self.pacer.landed();
                    tick.changed |= self.error.as_ref() != Some(&e);
                    self.error = Some(e);
                }
                Outcome::Done(line, ok, stopped) => {
                    tick.status.push(line);
                    tick.stopped.extend(stopped);
                    tick.rescan |= ok;
                    self.pacer.hurry();
                }
            }
        }
        for job in std::mem::take(&mut self.queued) {
            if !self.send(job) {
                tick.status.push("resources: no API in this window".into());
            }
        }
        if self.pacer.due(now, showing) && self.send(Job::Sample) {
            self.pacer.ask(now);
        }
        if self.worker.is_some() {
            tick.wake = self.pacer.wake_in(now, showing);
        }
        tick
    }
}

/// Whether the panel is on screen: chosen, in a visible inspector, in a visible window.
pub fn showing(s: &State) -> bool {
    s.visible && crate::panes::ViewItem::Resources.on(s)
}

/// Every poll: the panel's samples and quick actions. Returns whether the window should redraw.
pub fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    let show = showing(s);
    let tick = s.resources.tick(t, show);
    let changed = tick.changed || !tick.status.is_empty();
    if let Some(line) = tick.status.into_iter().last() {
        s.status = Some(line);
    }
    // A device that's out in its own window lets go of its connection once its shutdown worked.
    for udid in &tick.stopped {
        s.popouts.hub.borrow_mut().stop(udid);
    }
    if tick.rescan {
        s.rescan();
    }
    if let Some(secs) = tick.wake {
        tx.wake_after(Duration::from_secs_f64(secs));
    }
    changed
}

/// ⌘⌥R, the View menu's Resources and the inspector's gauge button: shows the panel (and the
/// inspector, if hidden), or swaps it back for the platform panel when it's on screen. It
/// changes nothing else, and nothing else changes it (#561): a scan, a boot or a selection
/// leaves it as it is.
pub fn toggle(s: &mut State, cause: Cause) {
    let on_screen = crate::panes::ViewItem::Resources.on(s);
    crate::panes::set(s, Target::Resources, !on_screen, cause);
    if !on_screen {
        crate::panes::show(s, Side::Inspector, cause);
    }
}

// ----- view -------------------------------------------------------------------------------------

fn green() -> Color {
    tok::accent(4, true)
}

fn orange() -> Color {
    tok::accent(3, true)
}

fn red() -> Color {
    tok::accent(2, true)
}

/// The colour of a level, a thermal state or a verdict, mildest first.
fn severity(word: &str) -> Color {
    match word {
        "normal" | "nominal" | "admit" => green(),
        "warning" | "fair" | "serious" | "hold" => orange(),
        _ => red(),
    }
}

/// `n` bytes as the panel shows them.
pub fn bytes(n: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    const MB: f64 = (1u64 << 20) as f64;
    let n = n as f64;
    if n >= 100.0 * GB {
        format!("{:.0} GB", n / GB)
    } else if n >= GB {
        format!("{:.1} GB", n / GB)
    } else {
        format!("{:.0} MB", n / MB)
    }
}

fn rate(per_s: Option<f64>) -> String {
    match per_s {
        Some(v) if v >= 1024.0 * 1024.0 => format!("{:.1} MB/s", v / (1024.0 * 1024.0)),
        Some(v) if v >= 1024.0 => format!("{:.0} KB/s", v / 1024.0),
        Some(_) => "<1 KB/s".into(),
        None => "–".into(),
    }
}

fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "–".into(), |v| format!("{v:.0}%"))
}

/// A horizontal bar `frac` full.
fn bar(frac: f64, color: Color, t: Theme) -> El {
    let frac = if frac.is_finite() { frac.clamp(0.0, 1.0) as f32 } else { 0.0 };
    row().h(6.0).radius(3.0).fill(t.control_off).child(el().h(6.0).radius(3.0).fill(color).grow(frac)).child(el().h(6.0).grow(1.0 - frac))
}

fn gauge(key: &str, label: &str, frac: Option<f64>, color: Color, value: String, t: Theme) -> El {
    let meter = match frac {
        Some(f) => bar(f, color, t).grow(1.0),
        None => el().grow(1.0),
    };
    row()
        .key(format!("gauge-{key}"))
        .w_full()
        .h(22.0)
        .gap(8.0)
        .center_y()
        .child(text(label.to_string()).font(type_::LABEL).color(t.text_secondary).w(58.0))
        .child(meter)
        .child(text(value).key(format!("gauge-{key}-value")).font(type_::MONO_CAPTION).color(t.text).w(124.0).truncate())
}

/// A coloured dot and a word.
fn chip(key: &str, word: &str, t: Theme) -> El {
    row()
        .key(key.to_string())
        .gap(5.0)
        .center_y()
        .child(el().size(8.0, 8.0).radius(4.0).fill(severity(word)))
        .child(text(word.to_string()).font(type_::LABEL).color(t.text))
}

fn host_gauges(h: &Host, t: Theme) -> El {
    let cores = h.cpus.max(1) as f64;
    let mut c = col().key("host-gauges").w_full().gap(2.0);
    c = c.child(gauge(
        "cpu",
        "CPU",
        h.cpu_pct.map(|p| p / (cores * 100.0)),
        tok::accent(0, true),
        format!("{} of {} cores", pct(h.cpu_pct.map(|p| p / cores)), h.cpus),
        t,
    ));
    let per_core = h.load_one.map(|l| l / cores);
    let load_color = match per_core {
        Some(p) if p >= 1.0 => red(),
        Some(p) if p >= 0.7 => orange(),
        _ => green(),
    };
    c = c.child(gauge(
        "load",
        "Load",
        per_core,
        load_color,
        h.load_one.map_or_else(|| "–".into(), |l| format!("{l:.2} · {:.2}/core", l / cores)),
        t,
    ));
    let level = h.memory_level.as_deref().unwrap_or("normal");
    let memory = match (&h.memory_level, h.memory_free_pct) {
        (Some(l), Some(f)) => format!("{l} · {f:.0}% free"),
        (Some(l), None) => l.clone(),
        (None, Some(f)) => format!("{f:.0}% free"),
        (None, None) => "–".into(),
    };
    c = c.child(gauge("memory", "Memory", h.memory_free_pct.map(|f| 1.0 - f / 100.0), severity(level), memory, t));
    let swap = if h.swap_total == 0 { "none".into() } else { format!("{} / {}", bytes(h.swap_used), bytes(h.swap_total)) };
    let swap_frac = (h.swap_total > 0).then(|| h.swap_used as f64 / h.swap_total as f64);
    c = c.child(gauge("swap", "Swap", swap_frac, tok::accent(1, true), swap, t));
    let disk_frac = match (h.disk_free, h.disk_total) {
        (Some(f), Some(total)) if total > 0 => Some(1.0 - f as f64 / total as f64),
        _ => None,
    };
    let disk_color = match disk_frac {
        Some(f) if f >= 0.95 => red(),
        Some(f) if f >= 0.85 => orange(),
        _ => tok::accent(5, true),
    };
    let disk = h.disk_free.map_or_else(|| "–".into(), |f| format!("{} free", bytes(f)));
    c = c.child(gauge("disk", "Disk", disk_frac, disk_color, disk, t));
    let thermal = match &h.thermal {
        Some(state) => chip("gauge-thermal-value", state, t),
        None => text("–").font(type_::MONO_CAPTION).color(t.text_secondary),
    };
    c.child(
        row()
            .key("gauge-thermal")
            .w_full()
            .h(22.0)
            .gap(8.0)
            .center_y()
            .child(text("Thermal").font(type_::LABEL).color(t.text_secondary).w(58.0))
            .child(thermal),
    )
}

/// A sparkline of `spark`, scaled to its own range: from 0 (CPU, `from_zero`) or from its lowest
/// value (memory, which moves little against its size), across at least `span`.
fn sparkline(key: String, spark: Option<&Spark>, span: f32, from_zero: bool, color: Color, t: Theme) -> El {
    const W: f32 = 64.0;
    const H: f32 = 20.0;
    let base = el().key(key.clone()).size(W, H).radius(radius::SM).fill(t.hover);
    let Some(spark) = spark.filter(|s| s.len() > 0) else { return base };
    let lo = if from_zero { 0.0 } else { spark.values().fold(f32::INFINITY, f32::min) };
    let hi = spark.values().fold(lo + span, f32::max);
    let n = spark.len();
    // Right-aligned: the newest value at the right edge, a minute across.
    let x0 = (HISTORY - n) as f32;
    let y = |v: f32| 1.0 + (H - 2.0) * (1.0 - ((v - lo) / (hi - lo)).clamp(0.0, 1.0));
    let mut points: Vec<[f32; 2]> = spark.values().enumerate().map(|(i, v)| [(x0 + i as f32) * W / (HISTORY - 1) as f32, y(v)]).collect();
    if points.len() == 1 {
        let [_, py] = points[0];
        points.insert(0, [W - 4.0, py]);
    }
    canvas(Shape::new().view_box(W, H).stroke(points, 1.25, color)).key(key).size(W, H).radius(radius::SM).fill(t.hover)
}

fn sort_button(sort: Sort, current: Sort, t: Theme) -> El {
    let (key, label) = match sort {
        Sort::Cpu => ("resources-sort-cpu", "CPU"),
        Sort::Memory => ("resources-sort-memory", "Memory"),
    };
    row()
        .key(key)
        .role("tab")
        .label(format!("Sort by {label}"))
        .h(22.0)
        .px(8.0)
        .center_y()
        .radius(radius::SM)
        .fill(if sort == current { tok::accent(0, true) } else { t.control_off })
        .on_press(move |s: &mut State| s.prefs.current.resources.sort = sort)
        .child(text(label).font(type_::LABEL).color(t.text))
}

fn device_row(s: &State, r: &Row, sort: Sort, t: Theme) -> El {
    let known = s.devices.iter().find(|d| d.udid.eq_ignore_ascii_case(&r.id)).map(|d| d.udid.clone());
    let selected = known.is_some() && known == s.selected;
    let history = s.resources.history.get(&r.id);
    let spark = match sort {
        Sort::Cpu => sparkline(format!("resource-spark-{}", r.id), history.map(|h| &h.cpu), 10.0, true, tok::accent(0, true), t),
        Sort::Memory => sparkline(format!("resource-spark-{}", r.id), history.map(|h| &h.memory), 64.0, false, tok::accent(1, true), t),
    };
    let platform = match r.platform.as_str() {
        "ios" => "iOS",
        "android" => "Android",
        _ => "",
    };
    let mut facts = format!("{} CPU · {}", r.cpu_pct.map_or_else(|| "–".into(), |c| format!("{c:.1}%")), bytes(r.memory));
    for (v, what) in [(r.read_per_s, "read"), (r.write_per_s, "written")] {
        if v.is_some_and(|v| v > 0.0) {
            facts.push_str(&format!(" · {} {what}", rate(v)));
        }
    }
    if let Some(w) = r.energy_w {
        facts.push_str(&format!(" · {w:.1} W"));
    }
    let mut actions = row().gap(4.0).center_y();
    if let Some(udid) = known.clone() {
        let label = if selected { "Shown in the window" } else { "Show in the window" };
        let mut b =
            crate::debug::icon_button(format!("resource-select-{}", r.id), &icons::REVEAL, label, false, t, move |s: &mut State| {
                s.actions.push(Action::Select(udid.clone()))
            });
        if selected {
            b = b.fill(tok::accent(0, true));
        }
        actions = actions.child(b);
    }
    if let Some(stream) = r.stream.clone() {
        let name = r.name.clone();
        actions = actions.child(crate::debug::icon_button(
            format!("resource-stream-{}", r.id),
            &icons::RECORD,
            "Stop the stream",
            false,
            t,
            move |s: &mut State| s.resources.queued.push(Job::StopStream { stream: stream.clone(), name: name.clone() }),
        ));
    }
    if let Some(udid) = known {
        let (platform, name) = (r.platform.clone(), r.name.clone());
        actions = actions.child(crate::debug::icon_button(
            format!("resource-shutdown-{}", r.id),
            &icons::POWER,
            "Shut down",
            false,
            t,
            move |s: &mut State| shut_down(s, &udid, &platform, &name),
        ));
    }
    col()
        .key(format!("resource-row-{}", r.id))
        .w_full()
        .gap(4.0)
        .padding(8.0)
        .radius(radius::SM)
        .fill(t.hover)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(text(r.name.clone()).font(type_::BODY).color(t.text).grow(1.0).shrink().min_w(0.0).truncate())
                .child(text(platform).font(type_::LABEL).color(t.text_secondary))
                .child(spark),
        )
        .child(
            row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(
                    text(facts)
                        .key(format!("resource-facts-{}", r.id))
                        .font(type_::MONO_CAPTION)
                        .color(t.text_secondary)
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .truncate(),
                )
                .child(actions),
        )
}

/// A row's Shut down. The device the window shows shuts down as the toolbar's button does, so its
/// screen and connection close with it; any other goes through the API. One out in its own window
/// lets go of its connection when the API says it shut down (see `poll`), so no frame of it
/// outlives the shutdown; if the call fails it keeps the connection and the error shows.
pub fn shut_down(s: &mut State, udid: &str, platform: &str, name: &str) {
    if !s.popouts.is_open(udid) && s.selected.as_deref() == Some(udid) && s.showing_screen() {
        s.actions.push(Action::Shutdown);
    } else {
        s.resources.queued.push(Job::Shutdown { device: udid.to_owned(), platform: platform.to_owned(), name: name.to_owned() });
    }
}

fn section(title: &str, t: Theme) -> El {
    text(title.to_string()).font(type_::SECTION).color(t.text_secondary).padding(2.0)
}

/// The panel, in the inspector's place.
pub fn panel(s: &State, t: Theme) -> El {
    let sort = s.prefs.current.resources.sort;
    let mut title = row().w_full().gap(8.0).center_y().child(text("RESOURCES").font(type_::SECTION).color(t.text_secondary));
    if let Some(v) = s.resources.reading.as_ref().and_then(|r| r.host.verdict.as_deref()) {
        title = title.child(chip("resources-verdict", v, t));
    }
    let mut body = col().w_full().gap(10.0);
    match &s.resources.reading {
        None => {
            let line = s.resources.error.clone().unwrap_or_else(|| "Sampling…".into());
            body = body.child(text(line).key("resources-empty").font(type_::BODY).color(t.text_secondary).wrap());
        }
        Some(r) => {
            body = body.child(section("HOST", t)).child(host_gauges(&r.host, t));
            let devices = row()
                .w_full()
                .gap(4.0)
                .center_y()
                .child(section("DEVICES", t))
                .child(el().grow(1.0))
                .child(sort_button(Sort::Cpu, sort, t))
                .child(sort_button(Sort::Memory, sort, t));
            let mut list = col().key("resource-rows").w_full().gap(6.0);
            for row in sorted(&r.rows, sort) {
                list = list.child(device_row(s, row, sort, t));
            }
            if r.rows.is_empty() {
                let line = r.unavailable.clone().unwrap_or_else(|| "No devices running".into());
                list = list.child(text(line).key("resources-no-devices").font(type_::BODY).color(t.text_secondary).wrap());
            }
            body = body.child(devices).child(list);
            if let Some(e) = &s.resources.error {
                body = body.child(text(e.clone()).font(type_::LABEL).color(t.text_secondary).wrap());
            }
        }
    }
    col()
        .key("resources-panel")
        .w(340.0)
        .min_w(340.0)
        .h_full()
        .padding(12.0)
        .gap(10.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(el().h(10.0))
        // Clear of the toggle pinned in the corner.
        .child(row().w_full().child(title.grow(1.0)).child(el().w(30.0)))
        .child(col().w_full().child(body).scroll(ScrollSpec { stick_bottom: false }).shrink().grow(1.0).min_h(0.0))
}

/// The gauge button pinned in the inspector's top-right corner.
fn toggle_button(s: &State, t: Theme) -> El {
    let on = s.prefs.current.resources.visible;
    let label = if on { "Hide resources" } else { "Show resources" };
    row()
        .key("resources-toggle")
        .role("button")
        .label(label)
        .size(26.0, 26.0)
        .center_y()
        .padding(5.0)
        .radius(radius::SM)
        .fill(if on { tok::accent(0, true) } else { t.control_off })
        .on_press(|s: &mut State| toggle(s, Cause::Button))
        .child(rune::icon(&icons::GAUGE, 16.0).color(t.text))
}

/// The inspector: the resource panel while chosen (with or without a device on screen),
/// otherwise the platform panel; either way with the toggle in its corner.
pub fn inspector(s: &State, t: Theme, platform: El) -> El {
    let panel = if s.prefs.current.resources.visible { panel(s, t) } else { platform };
    panel.child(toggle_button(s, t).pin(None, Some(10.0), Some(12.0), None))
}

// ----- scenes -----------------------------------------------------------------------------------

/// A minute of made-up samples: a wave around `mid`, `swing` either way.
fn wave(mid: f32, swing: f32, phase: f32) -> Spark {
    let mut s = Spark::default();
    for i in 0..HISTORY {
        let x = i as f32 / 6.0 + phase;
        s.push((mid + swing * (x.sin() * 0.7 + (x * 2.3).cos() * 0.3)).max(0.0).round());
    }
    s
}

/// Fixed data for the headless `resources` scene: a busy host, two devices from the demo list
/// (one streaming) and one stock emulator the window doesn't list.
pub fn demo_reading() -> Reading {
    const GB: u64 = 1 << 30;
    Reading {
        host: Host {
            cpus: 10,
            cpu_pct: Some(382.0),
            load_one: Some(4.21),
            memory_level: Some("warning".into()),
            memory_free_pct: Some(22.0),
            swap_used: 3 * GB / 2,
            swap_total: 4 * GB,
            disk_free: Some(182 * GB),
            disk_total: Some(994 * GB),
            thermal: Some("fair".into()),
            verdict: Some("hold".into()),
        },
        rows: vec![
            Row {
                id: "A1".into(),
                name: "iPhone 17".into(),
                platform: "ios".into(),
                cpu_pct: Some(31.7),
                memory: 1434 << 20,
                read_per_s: Some(0.0),
                write_per_s: Some(48.0 * 1024.0),
                energy_w: None,
                stream: Some("stream-1".into()),
            },
            Row {
                id: "pixel_9".into(),
                name: "pixel_9".into(),
                platform: "android".into(),
                cpu_pct: Some(61.2),
                memory: 2150 << 20,
                read_per_s: Some(1.5 * 1024.0 * 1024.0),
                write_per_s: Some(0.0),
                energy_w: None,
                stream: None,
            },
            Row {
                id: "qemu-system-aar pid 4182".into(),
                name: "qemu-system-aar pid 4182".into(),
                platform: "android".into(),
                cpu_pct: Some(12.4),
                memory: 3010 << 20,
                read_per_s: Some(0.0),
                write_per_s: Some(0.0),
                energy_w: None,
                stream: None,
            },
        ],
        unavailable: None,
    }
}

/// The headless `resources` scene: the demo with the panel showing made-up samples.
pub fn scene(s: &mut State) {
    crate::demo(s);
    s.prefs.current.resources.visible = true;
    let r = demo_reading();
    for (i, row) in r.rows.iter().enumerate() {
        let cpu = row.cpu_pct.unwrap_or(0.0) as f32;
        let mem = (row.memory >> 20) as f32;
        let phase = i as f32 * 1.7;
        s.resources.history.insert(row.id.clone(), History { cpu: wave(cpu, cpu * 0.35, phase), memory: wave(mem, mem * 0.04, phase) });
    }
    s.resources.reading = Some(r);
}

/// The same, sorted by memory.
pub fn memory_scene(s: &mut State) {
    scene(s);
    s.prefs.current.resources.sort = Sort::Memory;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn row(id: &str, cpu: Option<f64>, memory_mib: u64) -> Row {
        Row { id: id.into(), name: id.into(), platform: "ios".into(), cpu_pct: cpu, memory: memory_mib << 20, ..Row::default() }
    }

    fn ids(rows: Vec<&Row>) -> Vec<&str> {
        rows.into_iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn rows_sort_costliest_first() {
        let rows = vec![row("b", Some(10.0), 900), row("a", Some(50.0), 100), row("c", None, 4000), row("d", Some(10.0), 900)];
        assert_eq!(ids(sorted(&rows, Sort::Cpu)), ["a", "b", "d", "c"], "no rate yet goes last; ties by name");
        assert_eq!(ids(sorted(&rows, Sort::Memory)), ["c", "b", "d", "a"]);
        assert!(sorted(&[], Sort::Cpu).is_empty());
    }

    #[test]
    fn the_sparkline_ring_keeps_the_last_minute() {
        let mut s = Spark::default();
        assert_eq!(s.len(), 0);
        assert!(s.push(1.0));
        assert_eq!(s.values().collect::<Vec<_>>(), [1.0]);
        for i in 2..=70 {
            assert!(s.push(i as f32));
        }
        assert_eq!(s.len(), HISTORY);
        let v: Vec<f32> = s.values().collect();
        assert_eq!(v.first(), Some(&11.0), "the oldest ten went");
        assert_eq!(v.last(), Some(&70.0));
        assert!(v.windows(2).all(|w| w[1] == w[0] + 1.0), "oldest first, in order");
        // A full ring of one value doesn't change when that value comes again.
        let mut flat = Spark::default();
        for _ in 0..HISTORY {
            assert!(flat.push(5.0));
        }
        assert!(!flat.push(5.0), "the same line: no redraw");
        assert!(flat.push(6.0));
    }

    #[test]
    fn readings_come_from_the_api_answers() {
        let host = json!({
            "snapshot": {"cpus": 8, "load": {"one": 2.004, "five": 1.0, "fifteen": 1.0},
                "memory": {"level": "warning", "free_percent": 21.6, "total_bytes": 17_179_869_184u64,
                    "swap_used_bytes": 1_073_741_824u64, "swap_total_bytes": 2_147_483_648u64},
                "disk": {"free_bytes": 107_374_182_400u64, "total_bytes": 536_870_912_000u64}},
            "thermal_state": "nominal",
            "verdict": {"verdict": "hold"}
        });
        let usage = json!({
            "available": true,
            "host": {"cpus": 8, "cpu_pct": {"1s": 240.4, "1m": 200.0}},
            "devices": [
                {"id": "PIXEL", "name": "Pixel 9", "platform": "android", "kind": "emu", "cpu_pct": {"1s": 90.04, "1m": 80.0},
                 "resident_bytes": 2_000_000_000u64, "footprint_bytes": 0, "disk_read_bytes_per_s": {"1s": 2048.0}, "disk_write_bytes_per_s": {"1s": null}},
                {"id": "U1", "platform": "ios", "kind": "simulator", "cpu_pct": {"1s": null}, "resident_bytes": 5, "footprint_bytes": 1_048_576u64}
            ]
        });
        let streams = json!([
            {"stream_id": "stream-2", "device": "pixel", "state": "running"},
            {"stream_id": "stream-1", "device": "U1", "state": "ended"}
        ]);
        let r = reading(&host, &usage, &streams);
        assert_eq!(r.host.cpus, 8);
        assert_eq!(r.host.cpu_pct, Some(240.0));
        assert_eq!(r.host.load_one, Some(2.0));
        assert_eq!(r.host.memory_level.as_deref(), Some("warning"));
        assert_eq!((r.host.swap_used, r.host.swap_total), (1 << 30, 2 << 30));
        assert_eq!(r.host.thermal.as_deref(), Some("nominal"));
        assert_eq!(r.host.verdict.as_deref(), Some("hold"));
        assert_eq!(r.rows.len(), 2);
        assert_eq!((r.rows[0].name.as_str(), r.rows[0].cpu_pct), ("Pixel 9", Some(90.0)));
        assert_eq!(r.rows[0].memory, mib(2_000_000_000), "resident when there's no footprint");
        assert_eq!(r.rows[0].stream.as_deref(), Some("stream-2"), "an AVD's stream, matched without case");
        assert_eq!((r.rows[1].name.as_str(), r.rows[1].cpu_pct, r.rows[1].memory), ("U1", None, 1 << 20));
        assert_eq!(r.rows[1].stream, None, "an ended stream has nothing to stop");
        assert_eq!(r.unavailable, None);
        let none = reading(&host, &json!({"available": false, "reason": "not on this host", "devices": []}), &Value::Null);
        assert_eq!(none.unavailable.as_deref(), Some("not on this host"));
    }

    #[test]
    fn the_pacer_asks_once_a_second_and_never_while_hidden() {
        let mut p = Pacer::default();
        assert!(!p.due(0.0, false), "hidden: nothing");
        assert_eq!(p.wake_in(0.0, false), None, "hidden: no wake");
        assert!(p.due(0.0, true));
        p.ask(0.0);
        assert!(!p.due(5.0, true), "one at a time");
        assert_eq!(p.wake_in(0.2, true), None, "the worker wakes the window when it lands");
        p.landed();
        assert!(!p.due(0.5, true));
        assert_eq!(p.wake_in(0.25, true), Some(0.75));
        assert!(p.due(1.0, true));
        assert!(!p.due(1.0, false));
        p.hurry();
        assert!(p.due(0.1, true), "after a quick action, at once");
    }

    static SAMPLES: AtomicUsize = AtomicUsize::new(0);

    /// A stand-in API: counts `host` calls and answers with an empty host.
    fn counting_call(_: &mut Session, method: &str, _: &Value) -> Result<Value, String> {
        if method == "host" {
            SAMPLES.fetch_add(1, Ordering::SeqCst);
        }
        Ok(json!({"devices": []}))
    }

    /// Ticks at `now` until the sample in flight (if any) has landed.
    fn settle(r: &mut Resources, now: f64, showing: bool) -> Tick {
        let start = Instant::now();
        let mut tick = r.tick(now, showing);
        while r.pacer.in_flight() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the sample landed");
            std::thread::sleep(std::time::Duration::from_millis(1));
            let t = r.tick(now, showing);
            tick.changed |= t.changed;
            tick.status.extend(t.status);
            tick.rescan |= t.rescan;
            tick.wake = t.wake;
        }
        tick
    }

    #[test]
    fn sampling_stops_while_hidden() {
        let mut r = Resources::with_call(counting_call);
        // No waker (headless): never a thread, never a sample, never a wake.
        assert_eq!(r.tick(0.0, true), Tick::default());
        assert!(r.worker.is_none());
        r.connect(Arc::new(|| {}));
        // Hidden from the start: the thread doesn't even start.
        for i in 0..20 {
            assert_eq!(settle(&mut r, f64::from(i) * 0.25, false).wake, None);
        }
        assert!(r.worker.is_none(), "a panel never shown starts no thread");
        assert_eq!(SAMPLES.load(Ordering::SeqCst), 0);
        // Shown for ten seconds of a fake clock, polled four times a second: one sample a second.
        let mut t = 10.0;
        while t < 20.0 {
            settle(&mut r, t, true);
            t += 0.25;
        }
        let shown = SAMPLES.load(Ordering::SeqCst);
        assert_eq!(shown, 10, "once a second");
        assert!(r.reading.is_some());
        // Hidden for ten seconds: no sample and no wake, however often it's polled.
        while t < 30.0 {
            assert_eq!(settle(&mut r, t, false).wake, None);
            t += 0.25;
        }
        assert_eq!(SAMPLES.load(Ordering::SeqCst), shown, "nothing sampled while hidden");
        // Shown again: a sample at once, and the sparklines start afresh after the gap.
        settle(&mut r, t, true);
        assert_eq!(SAMPLES.load(Ordering::SeqCst), shown + 1);
        assert_eq!(settle(&mut r, t + 0.25, true).wake, Some(0.75));
    }

    static CALLS: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

    /// A stand-in API that records quick actions and fails one device's shutdown.
    fn recording_call(_: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
        if matches!(method, "shutdown" | "stream_stop") {
            CALLS.lock().map_err(|e| e.to_string())?.push((method.to_owned(), p.clone()));
        }
        if method == "shutdown" && p["device"] == "stuck" {
            return Err("device busy".into());
        }
        Ok(json!({}))
    }

    #[test]
    fn quick_actions_reach_the_api() {
        let mut r = Resources::with_call(recording_call);
        r.connect(Arc::new(|| {}));
        r.queued.push(Job::Shutdown { device: "pixel_9".into(), platform: "android".into(), name: "Pixel 9".into() });
        r.queued.push(Job::StopStream { stream: "stream-1".into(), name: "iPhone 17".into() });
        r.queued.push(Job::Shutdown { device: "stuck".into(), platform: "ios".into(), name: "Stuck".into() });
        // Hidden: the actions still go, no sample does.
        let start = Instant::now();
        let mut status = Vec::new();
        let mut rescan = false;
        while status.len() < 3 {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "every action answered");
            let t = r.tick(0.0, false);
            status.extend(t.status);
            rescan |= t.rescan;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(status, ["Pixel 9: shut down", "iPhone 17: stream stopped", "shut down Stuck: device busy"]);
        assert!(rescan, "a device stopped: the list is scanned again");
        let calls = CALLS.lock().expect("calls").clone();
        assert_eq!(
            calls,
            [
                ("shutdown".to_owned(), json!({"device": "pixel_9", "platform": "android"})),
                ("stream_stop".to_owned(), json!({"stream_id": "stream-1"})),
                ("shutdown".to_owned(), json!({"device": "stuck", "platform": "ios"})),
            ]
        );
        assert!(r.pacer.due(0.0, true), "the gauges follow at once");
        // Without a line to the API the actions say so.
        let mut offline = Resources::with_call(recording_call);
        offline.queued.push(Job::Sample);
        assert_eq!(offline.tick(0.0, true).status, ["resources: no API in this window"]);
    }

    #[test]
    fn a_sample_that_changes_nothing_draws_nothing() {
        let mut r = Resources::with_call(counting_call);
        let reading = demo_reading();
        assert!(r.apply(0.0, reading.clone()));
        for i in 1..HISTORY {
            assert!(r.apply(i as f64, reading.clone()), "the sparklines still fill");
        }
        assert!(!r.apply(HISTORY as f64, reading.clone()), "full and flat: the same frame");
        let mut busier = reading.clone();
        busier.rows[0].cpu_pct = Some(80.0);
        assert!(r.apply(HISTORY as f64 + 1.0, busier));
        // A device that stopped loses its sparkline; a gap starts them afresh.
        let mut fewer = reading.clone();
        fewer.rows.truncate(1);
        assert!(r.apply(HISTORY as f64 + 2.0, fewer));
        assert_eq!(r.history.len(), 1);
        assert!(r.apply(HISTORY as f64 + 60.0, reading));
        assert!(r.history.values().all(|h| h.cpu.len() == 1), "after the gap, one point each");
    }

    #[test]
    fn prefs_persist_in_the_settings() {
        let s = crate::settings::parse(r#"{"version": 1, "resources": {"visible": true, "sort": "memory"}}"#).expect("parses");
        assert_eq!(s.resources, Prefs { visible: true, sort: Sort::Memory });
        assert_eq!(crate::settings::parse("{}").expect("parses").resources, Prefs::default(), "an older file: hidden, by CPU");
        let mut settings = crate::settings::Settings::default();
        settings.resources.sort = Sort::Memory;
        let text = serde_json::to_string(&settings).expect("json");
        assert_eq!(crate::settings::parse(&text).expect("round trip").resources.sort, Sort::Memory);
    }

    mod window {
        use super::super::*;
        use rune::{Env, Key, KeyEvent, Mods, Runtime};

        fn runtime(scene: fn(&mut State)) -> Runtime<State> {
            let mut s = crate::init();
            scene(&mut s);
            let mut rt = Runtime::new(s, crate::view::view, (1240.0, 870.0), Env::new(true, 2.0));
            rt.on_key = Some(crate::on_key);
            rt.frame(0.0);
            rt
        }

        fn press(rt: &mut Runtime<State>, key: &str) {
            let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            rt.frame(1.0 / 60.0);
        }

        #[test]
        fn the_panel_shows_gauges_and_rows_by_the_chosen_sort() {
            let mut rt = runtime(scene);
            assert!(rt.find("resources-panel").is_some() && rt.find("debug-panel").is_none(), "in the inspector's place");
            for key in ["gauge-cpu", "gauge-load", "gauge-memory", "gauge-swap", "gauge-disk", "gauge-thermal", "resources-verdict"] {
                assert!(rt.find(key).is_some(), "{key}");
            }
            let order = |rt: &Runtime<State>| {
                let mut rows: Vec<(f32, String)> = rt
                    .elements()
                    .into_iter()
                    .filter_map(|e| Some((e.target[1], e.key?.strip_prefix("resource-row-")?.to_owned())))
                    .collect();
                rows.sort_by(|a, b| a.0.total_cmp(&b.0));
                rows.into_iter().map(|(_, k)| k).collect::<Vec<_>>()
            };
            assert_eq!(order(&rt), ["pixel_9", "A1", "qemu-system-aar pid 4182"]);
            press(&mut rt, "resources-sort-memory");
            assert_eq!(rt.state.prefs.current.resources.sort, Sort::Memory);
            assert_eq!(order(&rt), ["qemu-system-aar pid 4182", "pixel_9", "A1"]);
            // A device the window doesn't list has no quick actions.
            assert!(rt.find("resource-shutdown-qemu-system-aar pid 4182").is_none());
            assert!(rt.find("resource-select-qemu-system-aar pid 4182").is_none());
            assert!(rt.find("resource-stream-pixel_9").is_none(), "no stream, no stop");
        }

        #[test]
        fn quick_actions_queue_what_they_do() {
            let mut rt = runtime(scene);
            press(&mut rt, "resource-select-pixel_9");
            assert_eq!(rt.state.actions, [Action::Select("pixel_9".into())]);
            rt.state.actions.clear();
            press(&mut rt, "resource-shutdown-pixel_9");
            assert_eq!(
                rt.state.resources.queued,
                [Job::Shutdown { device: "pixel_9".into(), platform: "android".into(), name: "pixel_9".into() }],
                "a device not on screen goes through the API"
            );
            assert!(rt.state.actions.is_empty());
            press(&mut rt, "resource-shutdown-A1");
            assert_eq!(rt.state.actions, [Action::Shutdown], "the device on screen, as the toolbar's button");
            press(&mut rt, "resource-stream-A1");
            assert_eq!(rt.state.resources.queued.last(), Some(&Job::StopStream { stream: "stream-1".into(), name: "iPhone 17".into() }));
        }

        #[test]
        fn the_toggle_and_shortcut_show_and_hide_it() {
            let mut rt = runtime(crate::demo);
            assert!(rt.find("debug-panel").is_some() && rt.find("resources-panel").is_none());
            press(&mut rt, "resources-toggle");
            assert!(rt.state.prefs.current.resources.visible);
            assert!(rt.find("resources-panel").is_some());
            assert!(rt.find("resources-empty").is_some(), "no sample yet");
            let cmd_alt_r = KeyEvent { key: Key::Command('r'), mods: Mods { cmd: true, alt: true, ..Mods::default() } };
            rt.key(cmd_alt_r.clone());
            rt.frame(1.0 / 60.0);
            assert!(!rt.state.prefs.current.resources.visible);
            assert!(rt.find("debug-panel").is_some());
            // With the inspector hidden, ⌘⌥R brings it back with the panel in it.
            rt.state.prefs.current.inspector.visible = false;
            rt.key(cmd_alt_r);
            rt.frame(1.0 / 60.0);
            assert!(rt.state.prefs.current.inspector.visible && rt.find("resources-panel").is_some());
            let mut s = crate::init();
            s.prefs.current.resources.visible = true;
            assert!(showing(&s));
            s.visible = false;
            assert!(!showing(&s), "a hidden window samples nothing");
        }
    }
}
