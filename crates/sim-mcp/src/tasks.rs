//! Tasks (the `io.modelcontextprotocol/tasks` extension): a long method call run on its own thread,
//! with a handle the client polls (`tasks/get`) or is told about (`notifications/tasks`).
//!
//! Cancellation is cooperative, as the extension allows: a cancelled task is `cancelled` at once,
//! and the call sees the flag ([`Ctl`]) at its next safe point (#297). A `boot_many` starts no more
//! devices and shuts down those it was booting, then returns the ones that finished, which the
//! cancelled task keeps as its `result`; a `boot` shuts down the device it was booting; a `create`
//! or `clone` deletes what it made; a call only waiting (a boot held back by admission) stops
//! waiting. Any other result of a cancelled task is thrown away: the work that can't be interrupted
//! once running (an install, an erase) ends in the background. While it runs, the call's steps
//! (devices booted of all, an install's phases) are the task's status message.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::rpc::{INTERNAL_ERROR, META_SUBSCRIPTION, RpcError};

/// How long a task is kept, from its creation (the work isn't stopped when it passes).
const TTL: Duration = Duration::from_secs(3600);
/// How often a client should poll.
const POLL_MS: u64 = 1000;
/// How many tasks' calls may be running at once. A cancelled task's call keeps running (the API
/// can't interrupt it), so it counts until it ends.
pub const MAX_RUNNING: usize = 8;

/// Writes one outgoing JSON-RPC line.
pub type Sink = Arc<dyn Fn(String) + Send + Sync>;

enum State {
    Working,
    Completed(Value),
    Failed(Value),
    /// With the partial result the call returned after the cancel (a `boot_many`'s devices done).
    Cancelled(Option<Value>),
}

/// What a call last said about how far it is: its count of units (once it has counted any) and
/// its message, which is either that step's or a note on what it waits on since.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Note {
    pub done: u64,
    pub total: Option<u64>,
    pub message: String,
    /// The message is a step's (it says the count itself), not a note after one.
    pub counted: bool,
}

impl Note {
    /// The message, with the count after a note that doesn't say it; `None` before anything.
    pub fn say(&self) -> Option<String> {
        if self.message.is_empty() {
            return None;
        }
        Some(match self.total {
            Some(total) if !self.counted => format!("{} ({} of {total} done)", self.message, self.done),
            _ => self.message.clone(),
        })
    }
}

/// Told of each counted step of a call, on the call's thread.
pub type OnStep = Arc<dyn Fn(&Note) + Send + Sync>;

/// What a running call sees of its caller: whether the caller cancelled it, and what it last said
/// about how far it is (shown in the task's status and in progress messages).
#[derive(Clone, Default)]
pub struct Ctl {
    pub cancelled: Arc<AtomicBool>,
    pub note: Arc<Mutex<Note>>,
    on_step: Vec<OnStep>,
}

impl Ctl {
    /// This, also telling `f` of each counted step.
    pub fn on_step(mut self, f: OnStep) -> Ctl {
        self.on_step.push(f);
        self
    }

    /// The hooks that hand this to `sim-api`'s calls on the thread that runs them.
    pub fn hooks(&self) -> sim_api::CallHooks {
        let (cancelled, note, on_step) = (self.cancelled.clone(), self.note.clone(), self.on_step.clone());
        sim_api::CallHooks {
            cancelled: Box::new(move || cancelled.load(Ordering::Relaxed)),
            progress: Box::new(move |p: &sim_api::Progress| {
                let now = {
                    let mut n = note.lock().unwrap_or_else(|e| e.into_inner());
                    n.message = p.message.clone();
                    n.counted = p.total.is_some();
                    if let Some(total) = p.total {
                        (n.done, n.total) = (p.done, Some(total));
                    }
                    n.clone()
                };
                if now.counted {
                    for f in &on_step {
                        f(&now);
                    }
                }
            }),
        }
    }

    /// What the call last said, if anything.
    pub fn say(&self) -> Option<String> {
        self.note.lock().unwrap_or_else(|e| e.into_inner()).say()
    }
}

struct Entry {
    ctl: Ctl,
    method: String,
    created: SystemTime,
    updated: SystemTime,
    started: Instant,
    finished: Option<Instant>,
    state: State,
}

/// A client listening for status changes of some tasks (`subscriptions/listen` with `taskIds`).
struct Listener {
    subscription: Value,
    ids: HashSet<String>,
    sink: Sink,
}

#[derive(Default)]
struct Table {
    entries: HashMap<String, Entry>,
    listeners: Vec<Listener>,
}

/// The tasks of one server.
#[derive(Clone)]
pub struct Tasks {
    table: Arc<Mutex<Table>>,
    /// Calls running now, cancelled tasks' included.
    running: Arc<AtomicUsize>,
    ttl: Duration,
}

/// One running call, counted until it ends (or its thread can't start).
struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for Tasks {
    fn default() -> Tasks {
        Tasks::with_ttl(TTL)
    }
}

/// `2025-11-25T10:30:00Z` for a time.
pub fn iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A random 128-bit id as 32 hex digits: a task's, a page snapshot's. Nobody can guess another's.
pub(crate) fn new_id() -> String {
    let mut bytes = [0u8; 16];
    if sim_core::entropy::fill(&mut bytes).is_err() {
        // No OS entropy: mix the clock and the process's random hash keys instead.
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Tasks {
    pub fn with_ttl(ttl: Duration) -> Tasks {
        Tasks { table: Arc::default(), running: Arc::default(), ttl }
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The task as `tasks/get` and `notifications/tasks` carry it.
    fn detail(&self, id: &str, e: &Entry) -> Value {
        let secs = e.finished.unwrap_or_else(Instant::now).duration_since(e.started).as_secs();
        let mut task = json!({
            "taskId": id,
            "createdAt": iso8601(e.created),
            "lastUpdatedAt": iso8601(e.updated),
            "ttlMs": self.ttl.as_millis() as u64,
            "pollIntervalMs": POLL_MS,
        });
        let (status, message) = match &e.state {
            State::Working => {
                let note = e.ctl.say().map(|n| format!(": {n}")).unwrap_or_default();
                ("working", format!("{} has been running for {secs} s{note}", e.method))
            }
            State::Completed(result) => {
                task["result"] = result.clone();
                if result["isError"] == true {
                    let text = result["content"][0]["text"].as_str().unwrap_or("");
                    ("completed", format!("{} failed after {secs} s: {text}", e.method))
                } else {
                    ("completed", format!("{} finished in {secs} s", e.method))
                }
            }
            State::Failed(error) => {
                task["error"] = error.clone();
                ("failed", format!("{} failed: {}", e.method, error["message"].as_str().unwrap_or("internal error")))
            }
            State::Cancelled(Some(partial)) => {
                task["result"] = partial.clone();
                let done = e.ctl.say().map(|n| format!(" ({n})")).unwrap_or_default();
                ("cancelled", format!("cancelled by the client; {} stopped and returned what it had done{done}, in `result`", e.method))
            }
            State::Cancelled(None) => (
                "cancelled",
                format!(
                    "cancelled by the client; {} stops at its next safe point: a boot_many starts no more devices and shuts down \
                     those it was booting (its result keeps the ones done), a boot shuts down the device it was booting, a create or \
                     clone deletes what it made, a call waiting for admission stops waiting. Work that can't be interrupted once \
                     running (an install, an erase) finishes in the background and its result is discarded",
                    e.method
                ),
            ),
        };
        task["status"] = status.into();
        task["statusMessage"] = message.into();
        task
    }

    /// Drops finished tasks older than their time to live.
    fn sweep(&self, table: &mut Table) {
        let ttl = self.ttl;
        table.entries.retain(|_, e| e.finished.is_none() || e.started.elapsed() < ttl);
    }

    /// Starts `work` on its own thread as a task for `method`. Its return value is the task's
    /// result (a `CallToolResult`). The handle (a `CreateTaskResult`'s fields) is returned at once;
    /// with [`MAX_RUNNING`] calls running (cancelled tasks' too), it fails instead.
    pub fn create(&self, method: &str, work: impl FnOnce() -> Value + Send + 'static) -> Result<Value, String> {
        self.create_with(method, |_| work())
    }

    /// [`Tasks::create`], with the task's [`Ctl`] for the work: the flag `tasks/cancel` sets, and
    /// the note its status shows.
    pub fn create_with(&self, method: &str, work: impl FnOnce(Ctl) -> Value + Send + 'static) -> Result<Value, String> {
        let id = new_id();
        // Each counted step is news to the task's listeners (weakly held: the table holds the Ctl).
        let (table, ttl, step_id) = (Arc::downgrade(&self.table), self.ttl, id.clone());
        let ctl = Ctl::default().on_step(Arc::new(move |_: &Note| {
            if let Some(table) = table.upgrade() {
                Tasks { table, running: Arc::default(), ttl }.touch(&step_id);
            }
        }));
        let now = SystemTime::now();
        let (handle, running) = {
            let mut table = self.lock();
            self.sweep(&mut table);
            let running = self.running.load(Ordering::SeqCst);
            if running >= MAX_RUNNING {
                return Err(format!(
                    "{running} calls are already running (a cancelled task's call counts until it ends); wait for one to finish"
                ));
            }
            self.running.fetch_add(1, Ordering::SeqCst);
            let entry = Entry {
                ctl: ctl.clone(),
                method: method.to_owned(),
                created: now,
                updated: now,
                started: Instant::now(),
                finished: None,
                state: State::Working,
            };
            let handle = self.detail(&id, &entry);
            table.entries.insert(id.clone(), entry);
            (handle, Running(self.running.clone()))
        };
        let (tasks, task_id) = (self.clone(), id.clone());
        let spawned = std::thread::Builder::new().name(format!("simee-task-{}", &id[..8])).spawn(move || {
            let _running = running;
            let outcome = catch_unwind(AssertUnwindSafe(|| work(ctl)));
            tasks.finish(&task_id, outcome.map_err(|_| RpcError::new(INTERNAL_ERROR, "the task's thread panicked").to_json()));
        });
        if let Err(e) = spawned {
            self.finish(&id, Err(RpcError::new(INTERNAL_ERROR, format!("cannot start the task's thread: {e}")).to_json()));
        }
        Ok(handle)
    }

    /// A working task's call said how far it is: its listeners hear the new status message.
    fn touch(&self, id: &str) {
        let mut table = self.lock();
        let Some(entry) = table.entries.get_mut(id).filter(|e| matches!(e.state, State::Working)) else { return };
        entry.updated = SystemTime::now();
        self.announce(&table, id);
    }

    /// Records how a task ended, unless it was cancelled first, and tells its listeners. A
    /// cancelled task keeps the result its call returned when that result says it is partial
    /// (`cancelled: true`: what it finished before it stopped), and its listeners hear it again
    /// with that result; any other is discarded.
    fn finish(&self, id: &str, outcome: Result<Value, Value>) {
        let mut table = self.lock();
        let Some(entry) = table.entries.get_mut(id) else { return };
        if let (State::Cancelled(partial @ None), Ok(result)) = (&mut entry.state, &outcome)
            && result["structuredContent"]["cancelled"] == true
        {
            *partial = Some(result.clone());
            entry.updated = SystemTime::now();
            self.announce(&table, id);
            return;
        }
        if !matches!(entry.state, State::Working) {
            return;
        }
        entry.state = match outcome {
            Ok(result) => State::Completed(result),
            Err(error) => State::Failed(error),
        };
        entry.updated = SystemTime::now();
        entry.finished = Some(Instant::now());
        self.announce(&table, id);
    }

    /// Sends the task's state to each listener that asked about it. (Lines go out with the table
    /// locked, so they are in order; sinks only write a line.)
    fn announce(&self, table: &Table, id: &str) {
        let Some(entry) = table.entries.get(id) else { return };
        for l in table.listeners.iter().filter(|l| l.ids.contains(id)) {
            let mut params = self.detail(id, entry);
            params["_meta"] = json!({META_SUBSCRIPTION: l.subscription});
            (l.sink)(json!({"jsonrpc": "2.0", "method": "notifications/tasks", "params": params}).to_string());
        }
    }

    /// The task, for `tasks/get`.
    pub fn get(&self, id: &str) -> Option<Value> {
        let mut table = self.lock();
        self.sweep(&mut table);
        table.entries.get(id).map(|e| self.detail(id, e))
    }

    /// Cancels a working task; `false` for an id that isn't one. A task already finished stays as
    /// it ended.
    pub fn cancel(&self, id: &str) -> bool {
        let mut table = self.lock();
        let Some(entry) = table.entries.get_mut(id) else { return false };
        if matches!(entry.state, State::Working) {
            entry.ctl.cancelled.store(true, Ordering::Relaxed);
            entry.state = State::Cancelled(None);
            entry.updated = SystemTime::now();
            entry.finished = Some(Instant::now());
            self.announce(&table, id);
        }
        true
    }

    /// Starts telling `sink` about these tasks, under `subscription`. `acknowledge` is called first
    /// with the ids that exist, before any notification can be sent for them.
    pub fn subscribe(&self, subscription: Value, ids: &[String], sink: Sink, acknowledge: impl FnOnce(&[String])) {
        let mut table = self.lock();
        let known: Vec<String> = ids.iter().filter(|id| table.entries.contains_key(*id)).cloned().collect();
        acknowledge(&known);
        table.listeners.push(Listener { subscription, ids: known.into_iter().collect(), sink });
    }

    pub fn unsubscribe(&self, subscription: &Value) {
        self.lock().listeners.retain(|l| &l.subscription != subscription);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Sender, channel};

    use super::*;

    /// How long one side of a test waits for the other before it fails.
    const WAIT: Duration = Duration::from_secs(10);

    fn gated() -> (Sender<()>, impl FnOnce() -> Value + Send + 'static) {
        let (release, wait) = channel::<()>();
        (release, move || {
            wait.recv().ok();
            json!({"content": [{"type": "text", "text": "done"}], "isError": false})
        })
    }

    fn status(tasks: &Tasks, id: &str) -> String {
        tasks.get(id).expect("task")["status"].as_str().expect("status").to_owned()
    }

    fn wait_for(tasks: &Tasks, id: &str, want: &str) -> Value {
        for _ in 0..500 {
            let task = tasks.get(id).expect("task");
            if task["status"] == want {
                return task;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("task never became {want}");
    }

    #[test]
    fn a_task_works_then_completes_with_its_result() {
        let tasks = Tasks::default();
        let (release, work) = gated();
        let handle = tasks.create("boot", work).expect("create");
        let id = handle["taskId"].as_str().expect("id").to_owned();
        assert_eq!(
            (handle["status"].clone(), handle["ttlMs"].clone(), handle["pollIntervalMs"].clone()),
            (json!("working"), json!(3_600_000), json!(1000))
        );
        assert!(id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(tasks.get(&id).expect("task")["statusMessage"].as_str().is_some_and(|m| m.starts_with("boot has been running")));
        assert_eq!(status(&tasks, &id), "working");
        release.send(()).expect("release");
        let done = wait_for(&tasks, &id, "completed");
        assert_eq!(done["result"]["content"][0]["text"], "done");
        assert!(done["statusMessage"].as_str().is_some_and(|m| m.starts_with("boot finished in")));
        assert_eq!(done.get("error"), None);
        assert!(done["createdAt"].as_str().is_some_and(|t| t.ends_with('Z') && t.len() == 20));
    }

    #[test]
    fn a_tool_error_is_a_completed_task_and_a_panic_a_failed_one() {
        let tasks = Tasks::default();
        let handle =
            tasks.create("boot", || json!({"content": [{"type": "text", "text": "no such device"}], "isError": true})).expect("create");
        let done = wait_for(&tasks, handle["taskId"].as_str().expect("id"), "completed");
        assert_eq!(done["result"]["isError"], true);
        assert_eq!(done["statusMessage"].as_str().map(|m| m.contains("no such device")), Some(true));
        let handle = tasks.create("install", || -> Value { panic!("boom") }).expect("create");
        let failed = wait_for(&tasks, handle["taskId"].as_str().expect("id"), "failed");
        assert_eq!(failed["error"]["code"], INTERNAL_ERROR);
        assert_eq!(failed.get("result"), None);
    }

    #[test]
    fn cancelling_a_working_task_discards_its_result() {
        let tasks = Tasks::default();
        let (release, work) = gated();
        let id = tasks.create("boot", work).expect("create")["taskId"].as_str().expect("id").to_owned();
        assert!(tasks.cancel(&id));
        let cancelled = tasks.get(&id).expect("task");
        assert_eq!(cancelled["status"], "cancelled");
        assert!(cancelled["statusMessage"].as_str().is_some_and(|m| m.contains("finishes in the background")));
        // The call finishes; the task stays cancelled and carries no result.
        release.send(()).expect("release");
        std::thread::sleep(Duration::from_millis(50));
        let after = tasks.get(&id).expect("task");
        assert_eq!((after["status"].clone(), after.get("result")), (json!("cancelled"), None));
        // Cancelling again, or a finished task, changes nothing; an unknown id isn't a task.
        assert!(tasks.cancel(&id));
        assert!(!tasks.cancel("nope") && tasks.get("nope").is_none());
        let finished = tasks.create("boot", || json!({"content": []})).expect("create")["taskId"].as_str().expect("id").to_owned();
        wait_for(&tasks, &finished, "completed");
        assert!(tasks.cancel(&finished));
        assert_eq!(status(&tasks, &finished), "completed");
    }

    #[test]
    fn a_tasks_status_is_its_calls_last_step_and_a_cancelled_one_keeps_its_partial_result() {
        let tasks = Tasks::default();
        // Each side waits on the other with a deadline, never a bare barrier: a side that panics
        // fails the test instead of leaving the other waiting forever (#533).
        let (said_tx, said) = channel::<()>();
        let (go, go_rx) = channel::<()>();
        let id = tasks
            .create_with("boot_many", move |ctl| {
                sim_api::with_call_hooks(ctl.hooks(), || {
                    // Each says something, then waits while the test reads it.
                    sim_api::call_step("booted", 4, 8, "4 of 8 done");
                    said_tx.send(()).ok();
                    go_rx.recv_timeout(WAIT).expect("the test read the step");
                    sim_api::call_progress("boot_many: 2 waiting at 5 s, admission hold: memory");
                    said_tx.send(()).ok();
                    go_rx.recv_timeout(WAIT).expect("the test cancelled");
                    // Cancelled meanwhile: it stops and says what it finished.
                    assert!(sim_api::call_cancelled());
                    json!({"content": [{"type": "text", "text": "partial"}], "structuredContent": {"cancelled": true, "failed": 4},
                        "isError": false})
                })
            })
            .expect("create")["taskId"]
            .as_str()
            .expect("id")
            .to_owned();
        said.recv_timeout(WAIT).expect("the step");
        let message = tasks.get(&id).expect("task")["statusMessage"].as_str().expect("message").to_owned();
        assert!(message.starts_with("boot_many has been running for ") && message.ends_with(" s: 4 of 8 done"), "{message}");
        go.send(()).expect("go");
        said.recv_timeout(WAIT).expect("the note");
        // A note after a step keeps the count beside it.
        let message = tasks.get(&id).expect("task")["statusMessage"].as_str().expect("message").to_owned();
        assert!(message.ends_with("admission hold: memory (4 of 8 done)"), "{message}");
        assert!(tasks.cancel(&id));
        go.send(()).expect("go");
        let task = wait_for_result(&tasks, &id);
        assert_eq!((task["status"].as_str(), task["result"]["structuredContent"]["failed"].as_u64()), (Some("cancelled"), Some(4)));
        assert!(task["statusMessage"].as_str().is_some_and(|m| m.contains("returned what it had done")), "{task}");
    }

    /// The task once its call has returned a partial result.
    fn wait_for_result(tasks: &Tasks, id: &str) -> Value {
        for _ in 0..500 {
            let task = tasks.get(id).expect("task");
            if task.get("result").is_some() {
                return task;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no result");
    }

    #[test]
    fn listeners_hear_each_step_of_a_working_task() {
        let tasks = Tasks::default();
        let (release, wait) = channel::<()>();
        let id = tasks
            .create_with("install", move |ctl| {
                sim_api::with_call_hooks(ctl.hooks(), || {
                    wait.recv().ok();
                    sim_api::call_step("installing", 1, 2, "installing a.apk on phone (2.0 MB)");
                    json!({"content": [], "isError": false})
                })
            })
            .expect("create")["taskId"]
            .as_str()
            .expect("id")
            .to_owned();
        let (tx, rx) = channel::<String>();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |line| {
            tx.lock().expect("tx").send(line).ok();
        });
        tasks.subscribe(json!(1), std::slice::from_ref(&id), sink, |_| {});
        release.send(()).expect("release");
        let statuses: Vec<(String, String)> = (0..2)
            .map(|_| {
                let note: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).expect("note")).expect("json");
                (
                    note["params"]["status"].as_str().unwrap_or("").to_owned(),
                    note["params"]["statusMessage"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        assert_eq!(statuses[0].0, "working");
        assert!(statuses[0].1.ends_with(": installing a.apk on phone (2.0 MB)"), "{statuses:?}");
        assert_eq!(statuses[1].0, "completed");
    }

    #[test]
    fn listeners_hear_a_cancelled_task_get_its_partial_result() {
        let tasks = Tasks::default();
        let (release, wait) = channel::<()>();
        let id = tasks
            .create("boot_many", move || {
                wait.recv().ok();
                json!({"content": [], "structuredContent": {"cancelled": true, "failed": 3}, "isError": false})
            })
            .expect("create")["taskId"]
            .as_str()
            .expect("id")
            .to_owned();
        let (tx, rx) = channel::<String>();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |line| {
            tx.lock().expect("tx").send(line).ok();
        });
        tasks.subscribe(json!(1), std::slice::from_ref(&id), sink, |_| {});
        assert!(tasks.cancel(&id));
        release.send(()).expect("release");
        let notes: Vec<Value> =
            (0..2).map(|_| serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).expect("note")).expect("json")).collect();
        // Cancelled at once with nothing yet, then again with what the call had done.
        assert_eq!((notes[0]["params"]["status"].as_str(), notes[0]["params"].get("result")), (Some("cancelled"), None));
        assert_eq!(notes[1]["params"]["status"], "cancelled");
        assert_eq!(notes[1]["params"]["result"]["structuredContent"]["failed"], 3);
        assert!(notes[1]["params"]["statusMessage"].as_str().is_some_and(|m| m.contains("returned what it had done")), "{}", notes[1]);
    }

    #[test]
    fn at_most_eight_calls_run_at_once_even_if_their_tasks_are_cancelled() {
        let tasks = Tasks::default();
        let mut releases = Vec::new();
        for _ in 0..MAX_RUNNING {
            let (release, work) = gated();
            tasks.create("boot", work).expect("create");
            releases.push(release);
        }
        let (extra, work) = gated();
        assert!(tasks.create("boot", work).expect_err("full").contains("8 calls are already running"));
        drop(extra);
        // Cancelling doesn't stop the calls, so it doesn't make room: create and cancel in a loop
        // can't start more boots than the limit.
        let ids: Vec<String> = tasks.lock().entries.keys().cloned().collect();
        for id in &ids {
            assert!(tasks.cancel(id));
        }
        for _ in 0..3 {
            assert!(tasks.create("boot", || json!({"content": []})).is_err(), "still 8 calls running");
        }
        // When a call ends there is room again.
        releases.remove(0).send(()).expect("release");
        for _ in 0..500 {
            if tasks.create("boot", || json!({"content": []})).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(tasks.running.load(Ordering::SeqCst) <= MAX_RUNNING);
        for r in releases {
            r.send(()).ok();
        }
    }

    #[test]
    fn finished_tasks_expire_after_their_ttl() {
        let ttl = Duration::from_secs(60);
        let tasks = Tasks::with_ttl(ttl);
        let (release, work) = gated();
        let working = tasks.create("boot", work).expect("create")["taskId"].as_str().expect("id").to_owned();
        let done = tasks.create("boot", || json!({"content": []})).expect("create")["taskId"].as_str().expect("id").to_owned();
        wait_for(&tasks, &done, "completed");
        // Both are older than the time to live (their clocks are set back, so no test waits on a
        // timer): the finished one is swept, the one still working is not.
        let old = Instant::now().checked_sub(ttl + Duration::from_secs(1)).expect("the host has been up a minute");
        for entry in tasks.lock().entries.values_mut() {
            entry.started = old;
        }
        assert!(tasks.get(&done).is_none(), "swept");
        assert_eq!(status(&tasks, &working), "working");
        release.send(()).ok();
    }

    #[test]
    fn listeners_hear_about_the_tasks_they_named() {
        let tasks = Tasks::default();
        let (release, work) = gated();
        let id = tasks.create("boot", work).expect("create")["taskId"].as_str().expect("id").to_owned();
        let other = tasks.create("boot", || json!({"content": []})).expect("create")["taskId"].as_str().expect("id").to_owned();
        let (tx, rx) = channel::<String>();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |line| {
            tx.lock().expect("tx").send(line).ok();
        });
        let mut agreed = Vec::new();
        tasks.subscribe(json!(7), &[id.clone(), "unknown".to_owned()], sink, |known| agreed = known.to_vec());
        assert_eq!(agreed, std::slice::from_ref(&id));
        wait_for(&tasks, &other, "completed");
        release.send(()).expect("release");
        let line = rx.recv_timeout(Duration::from_secs(5)).expect("notification");
        let note: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(note["method"], "notifications/tasks");
        assert_eq!((note["params"]["taskId"].clone(), note["params"]["status"].clone()), (json!(id), json!("completed")));
        assert_eq!(note["params"]["_meta"][META_SUBSCRIPTION], 7);
        assert!(rx.try_recv().is_err(), "nothing about the task it didn't name");
        tasks.unsubscribe(&json!(7));
        assert!(tasks.lock().listeners.is_empty());
    }

    #[test]
    fn timestamps_are_iso_8601_utc() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(UNIX_EPOCH + Duration::from_secs(1_700_000_000)), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(UNIX_EPOCH + Duration::from_secs(4_102_444_799)), "2099-12-31T23:59:59Z");
        let (a, b) = (new_id(), new_id());
        assert_ne!(a, b);
    }
}
