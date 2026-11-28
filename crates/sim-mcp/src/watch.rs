//! Change notifications. The API has nothing to subscribe to, so a watcher re-reads what it was
//! asked to watch and speaks up when it differs: the same read a client would do, without the
//! client polling. Each kind of resource is re-read at its own pace, which is also the most
//! often it can be announced: a screen once a second by default (up to 30 with `SIMEE_MCP_SCREEN_FPS`), logs once a second, a process list
//! every couple of seconds, so a burst of changes is one notification with the latest state.
//!
//! A watcher runs only while something is subscribed (a connection with no subscriptions has no
//! thread and makes no reads), sleeps until its next read is due, and stops when dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::resources::{self, Target};

/// The environment variable that sets how often a subscribed screen is read, in frames a second.
pub const SCREEN_FPS_ENV: &str = "SIMEE_MCP_SCREEN_FPS";
/// The most screen frames a second a subscription can ask for.
const MAX_SCREEN_FPS: f64 = 30.0;

/// How often each kind of resource is re-read while subscribed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pace {
    /// A screen or screenshot: the most often `updated` is sent for one. Default 1 s.
    pub screen: Duration,
    /// Logs, which are batched: everything since the last notification is one.
    pub logs: Duration,
    /// The device list, and so `list_changed`.
    pub state: Duration,
    /// Everything else: a device's info, apps, processes and preferences.
    pub slow: Duration,
}

impl Default for Pace {
    fn default() -> Pace {
        Pace { screen: Duration::from_secs(1), logs: Duration::from_secs(1), state: Duration::from_secs(1), slow: Duration::from_secs(2) }
    }
}

impl Pace {
    /// Every kind at `every` (tests, and the old once-a-second rhythm).
    pub fn uniform(every: Duration) -> Pace {
        Pace { screen: every, logs: every, state: every, slow: every }
    }

    /// The default pace with the screen's set by `SIMEE_MCP_SCREEN_FPS` (frames a second, above 0
    /// and at most 30) when it is.
    pub fn from_env() -> Result<Pace, String> {
        match std::env::var(SCREEN_FPS_ENV) {
            Ok(v) => Pace::with_screen_fps(&v),
            Err(_) => Ok(Pace::default()),
        }
    }

    /// [`Pace::default`] with the screen at `fps` frames a second.
    pub fn with_screen_fps(fps: &str) -> Result<Pace, String> {
        let n: f64 = fps.trim().parse().map_err(|_| format!("{SCREEN_FPS_ENV}={fps}: expected a number of frames a second"))?;
        if !(n > 0.0 && n <= MAX_SCREEN_FPS) {
            return Err(format!("{SCREEN_FPS_ENV}={fps}: frames a second must be above 0 and at most {MAX_SCREEN_FPS}"));
        }
        Ok(Pace { screen: Duration::from_secs_f64(1.0 / n), ..Pace::default() })
    }

    fn of(&self, uri: &str) -> Duration {
        match resources::parse(uri) {
            Some(Target::Screen(_) | Target::Screenshot(_)) => self.screen,
            Some(Target::Logs(_)) => self.logs,
            Some(Target::Devices) => self.state,
            _ => self.slow,
        }
    }
}

/// Sends one notification: its method and params.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// What to watch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    /// Resources whose contents to watch (`notifications/resources/updated`).
    pub uris: Vec<String>,
    /// Whether to watch the list of resources (`notifications/resources/list_changed`), which
    /// changes when a device appears, disappears or starts or stops running.
    pub list_changed: bool,
}

/// What the resource list depends on: each device and whether it is running.
fn list_fingerprint(backend: &dyn Backend) -> Option<u64> {
    let devices = backend.call("devices", &json!({})).ok()?;
    let rows: Vec<String> = devices.as_array()?.iter().map(|d| format!("{}={}", d["udid"], d["state"])).collect();
    Some(hash_str(&rows.join(",")))
}

fn hash_str(text: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// One thing being watched: when it is next read, and what the last read of it was (`None`
/// before the first, the baseline, which announces nothing).
struct Item {
    /// The resource's URI in its one spelling; `None` for the resource list.
    uri: Option<String>,
    /// The spellings it was subscribed under, each announced when it changes.
    names: Vec<String>,
    period: Duration,
    due: Instant,
    seen: Option<Option<u64>>,
}

/// What is watched and when each is next read, apart from any thread or clock: the schedule is
/// driven by the `clock` it is given, so a test can step through seconds in no time.
pub struct Schedule {
    pace: Pace,
    items: Vec<Item>,
}

impl Schedule {
    pub fn new(pace: Pace) -> Schedule {
        Schedule { pace, items: Vec::new() }
    }

    /// Watches `spec` from `now` on. What was already watched keeps its schedule and its last
    /// read (a change since is still announced); what is new is read at once, as its baseline;
    /// what is gone is forgotten.
    pub fn set(&mut self, spec: &Spec, now: Instant) {
        // Spellings of one resource are one item, read once.
        let mut wanted: Vec<(Option<String>, Vec<String>)> = Vec::new();
        for name in &spec.uris {
            let key = resources::spelling(name).unwrap_or_else(|| name.clone());
            match wanted.iter_mut().find(|(k, _)| k.as_deref() == Some(key.as_str())) {
                Some((_, names)) if names.contains(name) => {}
                Some((_, names)) => names.push(name.clone()),
                None => wanted.push((Some(key), vec![name.clone()])),
            }
        }
        if spec.list_changed {
            wanted.push((None, Vec::new()));
        }
        self.items.retain(|item| wanted.iter().any(|(key, _)| *key == item.uri));
        for (key, names) in wanted {
            if let Some(item) = self.items.iter_mut().find(|item| item.uri == key) {
                item.names = names;
                continue;
            }
            let period = key.as_deref().map_or(self.pace.state, |u| self.pace.of(u));
            self.items.push(Item { uri: key, names, period, due: now, seen: None });
        }
    }

    /// When something is next due, or `None` when nothing is watched.
    pub fn next_due(&self) -> Option<Instant> {
        self.items.iter().map(|item| item.due).min()
    }

    /// Reads what is due, calling `emit` for each change. Each read is followed by one period
    /// measured from its end, so a slow read lowers the rate instead of piling up.
    pub fn step(&mut self, clock: &dyn Fn() -> Instant, backend: &dyn Backend, emit: &mut dyn FnMut(&str, Value)) {
        let now = clock();
        for item in self.items.iter_mut().filter(|item| item.due <= now) {
            let print = match &item.uri {
                Some(uri) => resources::fingerprint(backend, uri),
                None => list_fingerprint(backend),
            };
            if item.seen.is_some_and(|before| before != print) {
                if item.uri.is_none() {
                    emit("notifications/resources/list_changed", json!({}));
                }
                for name in &item.names {
                    emit("notifications/resources/updated", json!({"uri": name}));
                }
            }
            item.seen = Some(print);
            item.due = clock() + item.period;
        }
    }
}

struct Control {
    /// A new [`Spec`] for the thread to take up.
    spec: Option<Spec>,
    stop: bool,
}

struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    /// Set on drop, so a notification already on its way out is dropped too.
    stopped: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A running watcher; dropping it stops it.
pub struct Watcher {
    shared: Arc<Shared>,
}

impl Watcher {
    /// Watches `spec` instead of what it watched, keeping what is in both.
    pub fn update(&self, spec: Spec) {
        lock(&self.shared.control).spec = Some(spec);
        self.shared.wake.notify_one();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        lock(&self.shared.control).stop = true;
        self.shared.wake.notify_one();
    }
}

/// Starts watching `spec`, calling `emit` for each change at `pace`. The first read of each is
/// its baseline: nothing is sent for what is already there.
pub fn spawn(backend: Arc<dyn Backend>, spec: Spec, pace: Pace, emit: Emit) -> Watcher {
    let shared = Arc::new(Shared {
        control: Mutex::new(Control { spec: Some(spec), stop: false }),
        wake: Condvar::new(),
        stopped: AtomicBool::new(false),
    });
    let thread = shared.clone();
    let spawned = std::thread::Builder::new().name("simee-mcp-watch".into()).spawn(move || {
        let mut schedule = Schedule::new(pace);
        let mut send = |method: &str, params: Value| {
            if !thread.stopped.load(Ordering::Relaxed) {
                emit(method, params);
            }
        };
        loop {
            // Sleep until a read is due, waking early only to change what is watched or to stop.
            let mut control = lock(&thread.control);
            loop {
                if control.stop {
                    return;
                }
                if let Some(spec) = control.spec.take() {
                    schedule.set(&spec, Instant::now());
                }
                let wait = schedule.next_due().map(|due| due.saturating_duration_since(Instant::now()));
                control = match wait {
                    Some(left) if left.is_zero() => break,
                    Some(left) => thread.wake.wait_timeout(control, left).unwrap_or_else(|e| e.into_inner()).0,
                    None => thread.wake.wait(control).unwrap_or_else(|e| e.into_inner()),
                };
            }
            drop(control);
            schedule.step(&Instant::now, &*backend, &mut send);
        }
    });
    if let Err(e) = spawned {
        eprintln!("simee mcp: cannot start a watcher thread: {e}");
    }
    Watcher { shared }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::mpsc::{Receiver, channel};

    use super::*;
    use crate::backend::fake::Fake;

    const MS: Duration = Duration::from_millis(1);
    const SCREEN: &str = "simee://devices/phone/screen";
    const LOGS: &str = "simee://devices/phone/logs";
    const PROCESSES: &str = "simee://devices/phone/processes";

    /// A schedule on a clock the test moves, so seconds pass in no time.
    struct Stepped {
        schedule: Schedule,
        start: Instant,
        at: Cell<Duration>,
        sent: Vec<(Duration, String, Value)>,
    }

    impl Stepped {
        fn new(pace: Pace, spec: Spec) -> Stepped {
            let start = Instant::now();
            let mut schedule = Schedule::new(pace);
            schedule.set(&spec, start);
            Stepped { schedule, start, at: Cell::new(Duration::ZERO), sent: Vec::new() }
        }

        /// Runs the clock to `until` ms in 10 ms strides, stepping at each.
        fn run(&mut self, backend: &Fake, until: u64, mut each: impl FnMut(u64)) {
            let mut t = self.at.get().as_millis() as u64;
            while t <= until {
                self.at.set(Duration::from_millis(t));
                each(t);
                let (start, at) = (self.start, &self.at);
                let clock = move || start + at.get();
                let mut got = Vec::new();
                self.schedule.step(&clock, backend, &mut |m, p| got.push((m.to_owned(), p)));
                self.sent.extend(got.into_iter().map(|(m, p)| (at.get(), m, p)));
                t += 10;
            }
        }

        fn updates(&self, uri: &str) -> Vec<u64> {
            self.sent
                .iter()
                .filter(|(_, m, p)| m == "notifications/resources/updated" && p["uri"] == uri)
                .map(|(at, ..)| at.as_millis() as u64)
                .collect()
        }
    }

    fn spec(uris: &[&str], list_changed: bool) -> Spec {
        Spec { uris: uris.iter().map(|u| (*u).to_owned()).collect(), list_changed }
    }

    #[test]
    fn a_screen_that_changes_every_10_ms_is_announced_once_a_second_by_default() {
        let fake = Fake::with_devices();
        fake.answer("screenshot", Ok(json!({"png": "0"})));
        let mut run = Stepped::new(Pace::default(), spec(&[SCREEN], false));
        run.run(&fake, 3000, |t| fake.answer("screenshot", Ok(json!({"png": t.to_string()}))));
        // Read at 0 (the baseline), then every second: 3 announcements in 3 s, 4 reads in all.
        assert_eq!(run.updates(SCREEN), [1000, 2000, 3000]);
        assert_eq!(fake.called("screenshot").len(), 4);
    }

    #[test]
    fn the_screen_rate_is_configurable_and_a_still_screen_says_nothing() {
        let fake = Fake::with_devices();
        fake.answer("screenshot", Ok(json!({"png": "same"})));
        let pace = Pace::with_screen_fps("10").expect("fps");
        assert_eq!(pace.screen, 100 * MS);
        let mut run = Stepped::new(pace, spec(&[SCREEN], false));
        run.run(&fake, 1000, |_| {});
        assert_eq!(run.sent.len(), 0);
        assert_eq!(fake.called("screenshot").len(), 11, "read ten times a second");
        // Then it changes once: one announcement, at the next read.
        fake.answer("screenshot", Ok(json!({"png": "new"})));
        run.run(&fake, 1200, |_| {});
        assert_eq!(run.updates(SCREEN), [1100]);
    }

    #[test]
    fn logs_that_arrive_in_a_burst_are_one_notification_with_the_state_at_the_read() {
        let fake = Fake::with_devices();
        fake.answer("logs", Ok(json!(["first"])));
        let mut run = Stepped::new(Pace::default(), spec(&[LOGS], false));
        // Thirty lines in the second after the baseline, none after.
        run.run(&fake, 3000, |t| {
            if t > 0 && t <= 300 && t % 10 == 0 {
                let lines: Vec<String> = (0..t / 10).map(|n| format!("line {n}")).collect();
                fake.answer("logs", Ok(json!(lines)));
            }
        });
        assert_eq!(run.updates(LOGS), [1000]);
        assert_eq!(fake.called("logs").len(), 4, "once a second");
    }

    #[test]
    fn each_kind_keeps_its_own_pace_and_a_busy_process_list_stays_quiet() {
        let fake = Fake::with_devices();
        fake.answer("processes", Ok(json!([{"pid": 1, "path": "/a", "cpu": 0.0}])));
        fake.answer("info", Ok(json!({"n": 0})));
        let pace = Pace::default();
        let mut run = Stepped::new(pace, spec(&[PROCESSES, "simee://devices/phone"], true));
        run.run(&fake, 4000, |t| {
            // CPU moves all the time; a process starts at 2.5 s; the info changes at 3.5 s.
            let extra = if t >= 2500 { json!([{"pid": 2, "path": "/b"}]) } else { json!([]) };
            let mut rows = vec![json!({"pid": 1, "path": "/a", "cpu": t as f64})];
            rows.extend(extra.as_array().cloned().unwrap_or_default());
            fake.answer("processes", Ok(json!(rows)));
            if t >= 3500 {
                fake.answer("info", Ok(json!({"n": 1})));
            }
        });
        assert_eq!(
            run.updates(PROCESSES),
            [4000],
            "read at 0, 2 s and 4 s: the start at 2.5 s is seen at 4 s, and CPU moving is never announced"
        );
        assert_eq!(run.updates("simee://devices/phone"), [4000]);
        assert_eq!(fake.called("processes").len(), 3);
        assert_eq!(fake.called("devices").len(), 5, "the list, once a second");
        assert!(run.sent.iter().all(|(_, m, _)| m != "notifications/resources/list_changed"), "no device came or went");
    }

    #[test]
    fn a_device_that_boots_or_is_deleted_changes_the_list() {
        let fake = Fake::with_devices();
        let mut run = Stepped::new(Pace::default(), spec(&[], true));
        run.run(&fake, 2000, |_| {});
        assert!(run.sent.is_empty());
        // Boot the iPhone.
        fake.answer(
            "devices",
            Ok(json!([{"udid": "phone", "state": "Booted"}, {"udid": "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", "state": "Booted"}])),
        );
        run.run(&fake, 3000, |_| {});
        assert_eq!(run.sent.len(), 1);
        assert_eq!((run.sent[0].1.as_str(), run.sent[0].0), ("notifications/resources/list_changed", 3000 * MS));
        // Delete the Android one.
        fake.answer("devices", Ok(json!([{"udid": "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", "state": "Booted"}])));
        run.run(&fake, 4000, |_| {});
        assert_eq!(run.sent.len(), 2);
        // A device whose name or other fields change does not change the list.
        fake.answer("devices", Ok(json!([{"udid": "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", "state": "Booted", "name": "renamed"}])));
        run.run(&fake, 6000, |_| {});
        assert_eq!(run.sent.len(), 2);
    }

    #[test]
    fn unsubscribing_stops_its_reads_and_subscribing_keeps_the_others_baselines() {
        let fake = Fake::with_devices();
        fake.answer("apps", Ok(json!(["a"])));
        fake.answer("screenshot", Ok(json!({"png": "1"})));
        let apps = "simee://devices/phone/apps";
        let mut run = Stepped::new(Pace::uniform(100 * MS), spec(&[apps], false));
        run.run(&fake, 300, |_| {});
        // Apps changes, then a screen is added before apps is next read: apps is still announced.
        fake.answer("apps", Ok(json!(["a", "b"])));
        run.schedule.set(&spec(&[apps, SCREEN], false), run.start + 305 * MS);
        run.run(&fake, 500, |_| {});
        assert_eq!(run.updates(apps).len(), 1);
        assert!(run.updates(SCREEN).is_empty(), "a new subscription's first read is its baseline");
        // Drop apps: it is never read again.
        let reads = fake.called("apps").len();
        run.schedule.set(&spec(&[SCREEN], false), run.start + 505 * MS);
        run.run(&fake, 2000, |_| {});
        assert_eq!(fake.called("apps").len(), reads);
        // Nothing watched: nothing due, so a thread would sleep for good.
        run.schedule.set(&Spec::default(), run.start + 2005 * MS);
        assert_eq!(run.schedule.next_due(), None);
    }

    #[test]
    fn two_spellings_of_a_resource_are_one_read_a_period_and_each_is_announced() {
        let (a, b) = ("simee://device/phone/screen", "simee://devices/phone/screen");
        let fake = Fake::with_devices();
        fake.answer("screenshot", Ok(json!({"png": "0"})));
        let mut run = Stepped::new(Pace::default(), spec(&[a, b], false));
        run.run(&fake, 3000, |t| {
            if t == 1500 {
                fake.answer("screenshot", Ok(json!({"png": "changed"})));
            }
        });
        // Reads at 0, 1 s, 2 s and 3 s: one for the pair each time.
        assert_eq!(fake.called("screenshot").len(), 4);
        // The change at 1.5 s is seen by the read at 2 s and said under each spelling, once.
        assert_eq!(run.updates(a), [2000]);
        assert_eq!(run.updates(b), [2000]);
        // A schedule of one spelling reads exactly as often.
        let single = Fake::with_devices();
        single.answer("screenshot", Ok(json!({"png": "0"})));
        Stepped::new(Pace::default(), spec(&[b], false)).run(&single, 3000, |_| {});
        assert_eq!(single.called("screenshot").len(), 4);
    }

    #[test]
    fn the_screen_pace_comes_from_the_environment_setting() {
        assert_eq!(Pace::default().screen, 1000 * MS);
        assert_eq!(Pace::with_screen_fps("2").expect("2").screen, 500 * MS);
        assert_eq!(Pace::with_screen_fps(" 0.5 ").expect("half").screen, 2000 * MS);
        assert_eq!(Pace::with_screen_fps("30").expect("max").screen.as_micros(), 33_333);
        for bad in ["0", "-1", "31", "fast", "", "NaN"] {
            let err = Pace::with_screen_fps(bad).expect_err(bad);
            assert!(err.contains(SCREEN_FPS_ENV), "{err}");
        }
        // The other kinds keep their own pace.
        let fast = Pace::with_screen_fps("10").expect("10");
        assert_eq!((fast.logs, fast.state, fast.slow), (Pace::default().logs, Pace::default().state, Pace::default().slow));
    }

    // ---- The thread ----

    fn collecting() -> (Emit, Receiver<(String, Value)>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        (Arc::new(move |method, params| drop(lock(&tx).send((method.to_owned(), params)))), rx)
    }

    const TICK: Duration = Duration::from_millis(20);
    const WAIT: Duration = Duration::from_secs(5);
    const QUIET: Duration = Duration::from_millis(150);

    /// Waits for the first `method` call, failing after `WAIT` instead of spinning forever.
    fn until_called(fake: &Fake, method: &str) {
        let deadline = Instant::now() + WAIT;
        while fake.called(method).is_empty() {
            assert!(Instant::now() < deadline, "no {method} call within {WAIT:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until `method`'s call count holds still for `QUIET`, and returns it; fails after
    /// `WAIT` if the calls never stop.
    fn until_quiet(fake: &Fake, method: &str) -> usize {
        let deadline = Instant::now() + WAIT;
        let mut seen = fake.called(method).len();
        loop {
            assert!(Instant::now() < deadline, "{method} calls kept coming for {WAIT:?}: {seen}");
            std::thread::sleep(QUIET);
            let now = fake.called(method).len();
            if now == seen {
                return seen;
            }
            seen = now;
        }
    }

    #[test]
    fn a_changed_resource_is_announced_and_an_unchanged_one_is_not() {
        let fake = Arc::new(Fake::with_devices());
        fake.answer("info", Ok(json!({"state": "Shutdown"})));
        let (emit, rx) = collecting();
        let _watcher = spawn(fake.clone(), spec(&["simee://devices/phone", "simee://runtimes"], false), Pace::uniform(TICK), emit);
        // Nothing for what was already there (the first read, of both, is the baseline).
        until_called(&fake, "runtimes");
        assert!(rx.recv_timeout(QUIET).is_err());
        // Only the resource whose answer changed is announced.
        fake.answer("info", Ok(json!({"state": "Booted"})));
        let (method, params) = rx.recv_timeout(WAIT).expect("update");
        assert_eq!((method.as_str(), params["uri"].clone()), ("notifications/resources/updated", json!("simee://devices/phone")));
        // Once per change.
        assert!(rx.recv_timeout(QUIET).is_err());
    }

    #[test]
    fn the_resource_list_changes_when_a_device_starts_or_appears() {
        let fake = Arc::new(Fake::with_devices());
        let (emit, rx) = collecting();
        let _watcher = spawn(fake.clone(), spec(&[], true), Pace::uniform(TICK), emit);
        until_called(&fake, "devices");
        assert!(rx.recv_timeout(QUIET).is_err());
        fake.answer("devices", Ok(json!([{"udid": "phone", "name": "phone", "state": "Shutdown", "platform": "android"}])));
        assert_eq!(rx.recv_timeout(WAIT).expect("list changed").0, "notifications/resources/list_changed");
    }

    #[test]
    fn updating_a_running_watcher_adds_and_removes_what_it_reads() {
        let fake = Arc::new(Fake::with_devices());
        fake.answer("apps", Ok(json!(["a"])));
        let (emit, rx) = collecting();
        let watcher = spawn(fake.clone(), spec(&["simee://runtimes"], false), Pace::uniform(TICK), emit);
        until_called(&fake, "runtimes");
        watcher.update(spec(&["simee://runtimes", "simee://devices/phone/apps"], false));
        until_called(&fake, "apps");
        fake.answer("apps", Ok(json!(["a", "b"])));
        assert_eq!(rx.recv_timeout(WAIT).expect("apps").1["uri"], "simee://devices/phone/apps");
        // Drop runtimes: once its in-flight read is over, no more.
        watcher.update(spec(&["simee://devices/phone/apps"], false));
        let seen = until_quiet(&fake, "runtimes");
        std::thread::sleep(QUIET);
        assert_eq!(fake.called("runtimes").len(), seen);
    }

    #[test]
    fn a_dropped_watcher_stops_reading() {
        let fake = Arc::new(Fake::with_devices());
        let (emit, rx) = collecting();
        let watcher = spawn(fake.clone(), spec(&[], true), Pace::uniform(TICK), emit);
        drop(watcher);
        // The thread may be mid-read when it is dropped: wait until its reads have stopped for good.
        let calls = until_quiet(&fake, "devices");
        fake.answer("devices", Ok(json!([])));
        std::thread::sleep(QUIET);
        assert_eq!(fake.called("devices").len(), calls, "no more reads");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_watcher_between_reads_sleeps_instead_of_polling() {
        // A minute between reads: after the baseline, nothing runs. (The old watcher woke every
        // 10 ms to look at the clock, 100 times a second per subscriber.)
        let fake = Arc::new(Fake::with_devices());
        let (emit, _rx) = collecting();
        let _watcher = spawn(fake.clone(), spec(&[], true), Pace::uniform(Duration::from_secs(60)), emit);
        until_called(&fake, "devices");
        std::thread::sleep(QUIET);
        assert_eq!(fake.called("devices").len(), 1);
    }

    /// The cost of a subscription that nothing changes, as the watch thread's own wake-ups and
    /// CPU over `SECS` seconds, with a backend that answers at once (so it is the watcher's cost,
    /// not the device's): `cargo test --release -p simee-mcp idle_subscription -- --ignored --nocapture`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "a measurement, 10 s"]
    fn idle_subscription_cost() {
        const SECS: u64 = 10;
        // The thread named `simee-mcp-watch`: its voluntary context switches (each is a sleep
        // that ended) and its user + system clock ticks (100 a second).
        fn watch_thread() -> Option<(u64, u64)> {
            for entry in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
                let dir = entry.path();
                if std::fs::read_to_string(dir.join("comm")).ok()?.trim() != "simee-mcp-watch" {
                    continue;
                }
                let status = std::fs::read_to_string(dir.join("status")).ok()?;
                let switches = status.lines().find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))?.trim().parse().ok()?;
                let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
                let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
                let ticks = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
                return Some((switches, ticks));
            }
            None
        }
        for (what, uris, list_changed) in [
            ("one screen", vec![SCREEN.to_owned()], false),
            ("one screen and the resource list", vec![SCREEN.to_owned()], true),
            ("the resource list", vec![], true),
        ] {
            let fake = Arc::new(Fake::with_devices());
            fake.answer("screenshot", Ok(json!({"png": "AAAA"})));
            let (emit, _rx) = collecting();
            let watcher = spawn(fake.clone(), Spec { uris, list_changed }, Pace::default(), emit);
            std::thread::sleep(Duration::from_millis(500));
            let before = watch_thread().expect("the watch thread");
            std::thread::sleep(Duration::from_secs(SECS));
            let after = watch_thread().expect("the watch thread");
            drop(watcher);
            println!(
                "{what}: {} wake-ups a second, {} ms of CPU in {SECS} s, {} reads",
                (after.0 - before.0) as f64 / SECS as f64,
                (after.1 - before.1) * 10,
                fake.called("screenshot").len() + fake.called("devices").len(),
            );
        }
    }
}
