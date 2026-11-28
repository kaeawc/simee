//! The protocol: which request does what. Requests arrive as lines, each is answered on its own
//! thread, and everything the server says goes out through one [`Sink`], a line at a time.
//!
//! The current revision (2026-07-28) is stateless: every request names its version and carries its
//! client's capabilities in `_meta`, and `server/discover` says what the server offers. A client
//! of an older revision opens with `initialize` instead and is served in the older shape.

use std::collections::{BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::backend::{Backend, Executor};
use crate::rpc::{self, Ctx, EXT_TASKS, Era, INTERNAL_ERROR, Incoming, LEGACY, MODERN, RpcError};
use crate::tasks::{Ctl, Sink, Tasks};
use crate::tools::{self, Pages, Toolsets};
use crate::watch::{self, Emit, Pace, Spec, Watcher};
use crate::{prompts, resources};

/// What the server tells a client about using it.
const INSTRUCTIONS: &str = "simee drives iOS simulators and Android emulators. Call `devices` first; most tools take `device` \
(a UDID, a name, or `booted`). Every tool says which platforms it works on. `screenshot` returns the screen as an image; \
`tap`, `swipe`, `type` and `button` take device pixels. `boot`, `boot_many`, `create`, `clone`, `install`, `settle`, `erase` and \
`shutdown` can take a while: with the tasks extension they return a task to poll with tasks/get (cancel with tasks/cancel). \
Resources under simee://devices carry each device's info, screen (shrunk for you), logs and processes: subscribe to be told when one changes instead of polling; \
`device_view` opens a live, clickable view of a device. \
The tools listed are the toolsets this server was started with (SIMEE_MCP_TOOLSETS, default `core`); calling one that isn't listed \
says which toolset has it. `processes`, `logs`, `connections` and `devices_usage` page with `limit` and `cursor`.";

/// How often a call that blocks reports that it is still running, to a client that gave a progress token.
const HEARTBEAT: Duration = Duration::from_secs(2);

/// How long a client's list responses stay fresh: the tools are fixed for a build.
const STATIC_TTL_MS: u64 = 3_600_000;

fn server_info() -> Value {
    json!({"name": "simee", "version": env!("CARGO_PKG_VERSION")})
}

/// Timing knobs, shortened in tests.
#[derive(Clone, Copy)]
pub struct Timing {
    /// How often each kind of subscribed resource is re-read.
    pub watch: Pace,
    pub heartbeat: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing { watch: Pace::default(), heartbeat: HEARTBEAT }
    }
}

impl Timing {
    /// The defaults, with what the environment sets (`SIMEE_MCP_SCREEN_FPS`).
    pub fn from_env() -> Result<Timing, String> {
        Ok(Timing { watch: Pace::from_env()?, ..Timing::default() })
    }
}

/// A legacy client's subscriptions: the URIs, and the watcher of them while there are any.
#[derive(Default)]
struct Subscribed {
    uris: BTreeSet<String>,
    watcher: Option<Watcher>,
}

/// An open `subscriptions/listen`.
struct Listen {
    subscription: Value,
    _watcher: Option<Watcher>,
}

struct Inner {
    backend: Arc<dyn Backend>,
    sink: Sink,
    executor: Executor,
    tasks: Tasks,
    timing: Timing,
    /// The tools this server serves.
    toolsets: Toolsets,
    /// Snapshots of long lists, for their next pages.
    pages: Arc<Pages>,
    /// The capabilities a legacy client sent in `initialize` and the revision agreed, once one has.
    legacy: Mutex<Option<(Value, &'static str)>>,
    /// Requests being worked on, by id: set to true to say the client no longer wants the answer.
    inflight: Mutex<HashMap<String, Arc<AtomicBool>>>,
    running: Mutex<usize>,
    idle: Condvar,
    listens: Mutex<HashMap<String, Listen>>,
    /// A legacy client's `resources/subscribe` URIs, watched by one watcher that exists while there
    /// are any.
    subscribed: Mutex<Subscribed>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The MCP server over one connection.
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

impl Server {
    /// A server of `toolsets` (the default, [`Toolsets::default`], is `core`).
    pub fn new(backend: Arc<dyn Backend>, sink: Sink, toolsets: Toolsets) -> Server {
        Server::with_timing(backend, sink, Timing::default(), toolsets)
    }

    pub fn with_timing(backend: Arc<dyn Backend>, sink: Sink, timing: Timing, toolsets: Toolsets) -> Server {
        Server {
            inner: Arc::new(Inner {
                backend,
                sink,
                executor: Executor::new(),
                tasks: Tasks::default(),
                timing,
                toolsets,
                pages: Arc::default(),
                legacy: Mutex::new(None),
                inflight: Mutex::default(),
                running: Mutex::new(0),
                idle: Condvar::new(),
                listens: Mutex::default(),
                subscribed: Mutex::default(),
            }),
        }
    }

    /// Handles one line from the client. Requests are answered on other threads.
    pub fn handle_line(&self, line: &str) {
        let inner = &self.inner;
        match rpc::parse(line) {
            Err(error) => inner.reply(&Value::Null, Err(error), None),
            Ok(Incoming::Notification { method, params }) => inner.notification(&method, &params),
            Ok(Incoming::Request { id, method, params }) => {
                let answered = catch_unwind(AssertUnwindSafe(|| inner.request(id.clone(), &method, params)));
                if answered.is_err() {
                    inner.reply(&id, Err(RpcError::new(INTERNAL_ERROR, format!("`{method}` panicked"))), None);
                }
            }
        }
    }

    /// Waits until the requests in flight have been answered, or `timeout` passes. (Tasks and
    /// subscriptions aren't requests in flight.)
    pub fn drain(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut running = lock(&self.inner.running);
        while *running > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return };
            running = self.inner.idle.wait_timeout(running, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// A result carrying how long a client may cache it.
fn cacheable(mut result: Value, ttl_ms: u64, scope: &str) -> Value {
    result["ttlMs"] = ttl_ms.into();
    result["cacheScope"] = scope.into();
    result
}

/// The list entries in `value` under `key` that are strings (`taskIds`, `resourceSubscriptions`).
fn strings(value: &Value, key: &str) -> Vec<String> {
    value.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

/// Reports a blocking call's progress until dropped: each step the call counts (`progress` and
/// `total` in its units: devices booted of all, an install's phases) as it happens, and between
/// them a heartbeat every `every`, so a client that resets its timeout on progress keeps waiting.
/// The value only ever increases, as MCP requires: a heartbeat before any step counts seconds (the
/// fallback for a call that counts nothing), one after a step moves partway to the next unit
/// (`done` + 0.9·k/(k+1) for the k-th beat), and a call whose first progress was seconds stays in
/// seconds. Dropping it waits out a progress being sent, and none is sent after, so a call's
/// progress never follows its response (which MCP forbids).
struct Heartbeat {
    report: Arc<Mutex<Report>>,
}

/// What a [`Heartbeat`] has sent, under the one lock every send and the stop take.
struct Report {
    /// Set once the call has returned.
    stopped: bool,
    sink: Sink,
    token: Value,
    tool: String,
    start: Instant,
    /// The last value sent, if any.
    last: Option<f64>,
    /// Whether the values are the call's own units (else seconds).
    counted: bool,
    /// Heartbeats since the last step.
    beats: u32,
    note: crate::tasks::Note,
    before_send: Box<dyn Fn() + Send>,
}

impl Report {
    /// Sends `value` (with the call's total once counted) unless it wouldn't increase.
    fn send(&mut self, value: f64, message: String) {
        if self.stopped || self.last.is_some_and(|last| value <= last) {
            return;
        }
        (self.before_send)();
        let mut params = json!({"progressToken": self.token, "progress": value, "message": message});
        if let (true, Some(total)) = (self.counted, self.note.total) {
            params["total"] = total.into();
        }
        (self.sink)(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}).to_string());
        self.last = Some(value);
    }

    fn step(&mut self, note: &crate::tasks::Note) {
        self.note = note.clone();
        if self.last.is_none() {
            self.counted = true;
        }
        if self.counted {
            self.beats = 0;
            let message = format!("{}: {}", self.tool, note.message);
            self.send(note.done as f64, message);
        }
    }

    fn beat(&mut self, said: Option<String>) {
        let secs = self.start.elapsed().as_secs_f64().round();
        let note = said.map(|n| format!(": {n}")).unwrap_or_default();
        let message = format!("{} is still running ({secs} s){note}", self.tool);
        let value = if self.counted {
            self.beats += 1;
            let k = f64::from(self.beats);
            self.note.done as f64 + 0.9 * k / (k + 1.0)
        } else {
            secs
        };
        self.send(value, message);
    }
}

impl Heartbeat {
    /// Where the call's counted steps go: sent at once, on the call's thread.
    fn steps(&self) -> crate::tasks::OnStep {
        let report = self.report.clone();
        Arc::new(move |note: &crate::tasks::Note| lock(&report).step(note))
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        lock(&self.report).stopped = true;
    }
}

fn heartbeat(sink: Sink, token: Value, tool: String, every: Duration, ctl: Ctl) -> Heartbeat {
    heartbeat_noting(sink, token, tool, every, ctl, || {})
}

/// [`heartbeat`], calling `before_send` just before each progress goes out (tests pause there).
#[cfg(test)]
fn heartbeat_with(sink: Sink, token: Value, tool: String, every: Duration, before_send: impl Fn() + Send + 'static) -> Heartbeat {
    heartbeat_noting(sink, token, tool, every, Ctl::default(), before_send)
}

/// [`heartbeat_with`], saying what the call last said (its note in `ctl`) after the time.
fn heartbeat_noting(
    sink: Sink,
    token: Value,
    tool: String,
    every: Duration,
    ctl: Ctl,
    before_send: impl Fn() + Send + 'static,
) -> Heartbeat {
    let report = Arc::new(Mutex::new(Report {
        stopped: false,
        sink,
        token,
        tool,
        start: Instant::now(),
        last: None,
        counted: false,
        beats: 0,
        note: crate::tasks::Note::default(),
        before_send: Box::new(before_send),
    }));
    let shared = report.clone();
    let spawned = std::thread::Builder::new().name("simee-mcp-progress".into()).spawn(move || {
        let mut next = Instant::now() + every;
        while !lock(&shared).stopped {
            std::thread::sleep(Duration::from_millis(10));
            if Instant::now() < next {
                continue;
            }
            next += every;
            // Read before the report's lock: the call's thread takes the note's lock, then this one.
            let said = ctl.say();
            // Checked and sent under the one lock the drop takes, so a stop can't fall in between.
            lock(&shared).beat(said);
        }
    });
    if let Err(e) = spawned {
        eprintln!("simee mcp: cannot start a progress thread: {e}");
    }
    Heartbeat { report }
}

impl Inner {
    fn say(&self, message: &Value) {
        (self.sink)(message.to_string());
    }

    /// Answers a request. A modern result says it is complete and who sent it; an answer to a
    /// request that wasn't understood far enough to know its era is as is.
    fn reply(&self, id: &Value, outcome: Result<Value, RpcError>, ctx: Option<&Ctx>) {
        let message = match outcome {
            Ok(mut result) => {
                if ctx.is_some_and(Ctx::modern) && result.is_object() {
                    if result.get("resultType").is_none() {
                        result["resultType"] = "complete".into();
                    }
                    result["_meta"][rpc::META_SERVER_INFO] = server_info();
                } else if let Some(fields) = result.as_object_mut().filter(|_| ctx.is_some()) {
                    // The cache hints are the current revision's.
                    fields.remove("ttlMs");
                    fields.remove("cacheScope");
                }
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error.to_json()}),
        };
        self.say(&message);
    }

    /// What a request says about its era and client. Everything but `server/discover` and
    /// `initialize` needs to say which revision it speaks (or follow an `initialize`).
    fn context(&self, method: &str, params: &Value) -> Result<Ctx, RpcError> {
        let progress_token = rpc::meta(params).get("progressToken").cloned();
        match rpc::requested_version(params) {
            Some(MODERN) => {
                let caps = rpc::meta(params).get(rpc::META_CLIENT_CAPS).cloned().unwrap_or(Value::Null);
                Ok(Ctx { era: Era::Modern, version: MODERN, caps, progress_token })
            }
            Some(other) => Err(rpc::unsupported_version(other)),
            None if method == "server/discover" => Ok(Ctx { era: Era::Modern, version: MODERN, caps: Value::Null, progress_token }),
            None => match &*lock(&self.legacy) {
                Some((caps, version)) => Ok(Ctx { era: Era::Legacy, version, caps: caps.clone(), progress_token }),
                None => Err(RpcError::invalid_params(format!(
                    "`{method}` needs `_meta[\"{}\"]` (\"{MODERN}\"); or open with `initialize`, or probe with `server/discover`",
                    rpc::META_VERSION
                ))),
            },
        }
    }

    fn discover(&self) -> Value {
        let versions: Vec<&str> = std::iter::once(MODERN).chain(LEGACY.iter().copied()).collect();
        cacheable(
            json!({"supportedVersions": versions,
                "capabilities": {"tools": {}, "resources": {"subscribe": true, "listChanged": true},
                    "prompts": {"listChanged": false}, "extensions": {EXT_TASKS: {}}},
                "instructions": INSTRUCTIONS}),
            STATIC_TTL_MS,
            "public",
        )
    }

    /// A legacy client's opening request: the revision they asked for if it is one this server
    /// speaks, else the newest of them.
    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
        let version = LEGACY.iter().find(|v| **v == requested).unwrap_or(&LEGACY[0]);
        *lock(&self.legacy) = Some((params.get("capabilities").cloned().unwrap_or(Value::Null), version));
        json!({"protocolVersion": version,
            "capabilities": {"tools": {}, "resources": {"subscribe": true}, "prompts": {"listChanged": false}},
            "serverInfo": server_info(), "instructions": INSTRUCTIONS})
    }

    fn notification(&self, method: &str, params: &Value) {
        if method != "notifications/cancelled" {
            return;
        }
        let Some(request) = params.get("requestId") else { return };
        let key = rpc::id_key(request);
        if let Some(flag) = lock(&self.inflight).get(&key) {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(listen) = lock(&self.listens).remove(&key) {
            self.tasks.unsubscribe(&listen.subscription);
        }
    }

    fn request(self: &Arc<Self>, id: Value, method: &str, params: Value) {
        match method {
            "initialize" => return self.reply(&id, Ok(self.initialize(&params)), None),
            "ping" => return self.reply(&id, Ok(json!({})), None),
            _ => {}
        }
        let ctx = match self.context(method, &params) {
            Ok(ctx) => ctx,
            Err(error) => return self.reply(&id, Err(error), None),
        };
        let reply = |outcome: Result<Value, RpcError>| self.reply(&id, outcome, Some(&ctx));
        match method {
            "server/discover" => reply(Ok(self.discover())),
            "tools/list" => reply(Ok(cacheable(json!({"tools": tools::list(&self.toolsets, &ctx)}), STATIC_TTL_MS, "public"))),
            "resources/templates/list" => {
                reply(Ok(cacheable(json!({"resourceTemplates": resources::templates(&self.toolsets)}), STATIC_TTL_MS, "public")));
            }
            "tools/call" => self.tools_call(id.clone(), &params, &ctx),
            "prompts/list" => reply(prompts::list(&params, &ctx).map(|page| cacheable(page, STATIC_TTL_MS, "public"))),
            "prompts/get" => reply(prompts::get(&params, &self.toolsets)),
            "resources/list" => self.offload(id.clone(), ctx.clone(), |i| {
                Ok(cacheable(json!({"resources": resources::list(&*i.backend, &i.toolsets)}), 0, "private"))
            }),
            "resources/read" => match params.get("uri").and_then(Value::as_str) {
                Some(uri) => {
                    let uri = uri.to_owned();
                    let era = ctx.era;
                    self.offload(id.clone(), ctx.clone(), move |i| {
                        resources::read(&*i.backend, &i.toolsets, &uri)
                            .map(|contents| cacheable(json!({"contents": contents}), 0, "private"))
                            .map_err(|error| rpc::for_era(error, era))
                    });
                }
                None => reply(Err(RpcError::invalid_params("`uri` is required"))),
            },
            "tasks/get" | "tasks/update" | "tasks/cancel" if ctx.modern() => reply(self.task_request(method, &params)),
            "subscriptions/listen" if ctx.modern() => {
                if let Err(error) = self.listen(&id, &params, &ctx) {
                    reply(Err(error));
                }
            }
            "resources/subscribe" | "resources/unsubscribe" if !ctx.modern() => reply(self.legacy_subscription(method, &params)),
            other => reply(Err(RpcError::new(rpc::METHOD_NOT_FOUND, format!("Method not found: {other}")))),
        }
    }

    /// Runs `work` on a worker thread and answers with its outcome, unless the client cancelled the
    /// request meanwhile (then nothing more is sent for it).
    fn offload(self: &Arc<Self>, id: Value, ctx: Ctx, work: impl FnOnce(&Inner) -> Result<Value, RpcError> + Send + 'static) {
        self.offload_with(id, ctx, |inner, _| work(inner));
    }

    /// [`Server::offload`], with the request's [`Ctl`] for the work: its cancel flag is the one
    /// `notifications/cancelled` sets.
    fn offload_with(self: &Arc<Self>, id: Value, ctx: Ctx, work: impl FnOnce(&Inner, &Ctl) -> Result<Value, RpcError> + Send + 'static) {
        let key = rpc::id_key(&id);
        let ctl = Ctl::default();
        let cancelled = ctl.cancelled.clone();
        lock(&self.inflight).insert(key.clone(), cancelled.clone());
        *lock(&self.running) += 1;
        let inner = self.clone();
        self.executor.submit(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| work(&inner, &ctl)))
                .unwrap_or_else(|_| Err(RpcError::new(INTERNAL_ERROR, "the request's thread panicked")));
            lock(&inner.inflight).remove(&key);
            if !cancelled.load(Ordering::Relaxed) {
                inner.reply(&id, outcome, Some(&ctx));
            }
            *lock(&inner.running) -= 1;
            inner.idle.notify_all();
        });
    }

    fn tools_call(self: &Arc<Self>, id: Value, params: &Value, ctx: &Ctx) {
        let (name, args) = match tools::parse_call(params, &self.toolsets) {
            Ok(call) => call,
            Err(error) => return self.reply(&id, Err(error), Some(ctx)),
        };
        if tools::as_task(&name, ctx) {
            let (backend, pages, tool) = (self.backend.clone(), self.pages.clone(), name.clone());
            let created = self.tasks.create_with(&name, move |ctl| {
                sim_api::with_call_hooks(ctl.hooks(), || tools::result(&tool, tools::run(&*backend, &pages, &tool, &args), Era::Modern))
            });
            let result = match created {
                Ok(mut handle) => {
                    handle["resultType"] = "task".into();
                    handle
                }
                Err(message) => tools::result(&name, Err(message), ctx.era),
            };
            return self.reply(&id, Ok(result), Some(ctx));
        }
        let progress = ctx.progress_token.clone().filter(|_| tools::LONG_RUNNING.contains(&name.as_str()));
        let era = ctx.era;
        self.offload_with(id, ctx.clone(), move |inner, ctl| {
            let beat = progress.map(|token| heartbeat(inner.sink.clone(), token, name.clone(), inner.timing.heartbeat, ctl.clone()));
            // The call's own steps are sent as it reaches them, before its response (the beat is
            // dropped, and stops, before this returns).
            let ctl = match &beat {
                Some(beat) => ctl.clone().on_step(beat.steps()),
                None => ctl.clone(),
            };
            let ran = sim_api::with_call_hooks(ctl.hooks(), || tools::run(&*inner.backend, &inner.pages, &name, &args));
            drop(beat);
            Ok(tools::result(&name, ran, era))
        });
    }

    fn task_request(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let id = params.get("taskId").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("`taskId` is required"))?;
        let unknown = || RpcError::invalid_params(format!("Unknown task: {id}"));
        match method {
            "tasks/get" => self.tasks.get(id).ok_or_else(unknown),
            // Nothing a task does needs the client's input, so there is nothing to update.
            "tasks/update" => self.tasks.get(id).map(|_| json!({})).ok_or_else(unknown),
            _ => {
                if self.tasks.cancel(id) {
                    Ok(json!({}))
                } else {
                    Err(unknown())
                }
            }
        }
    }

    /// `subscriptions/listen`: acknowledges what the server will send, then sends it until the
    /// client cancels the request. The request gets no response while it is open.
    fn listen(self: &Arc<Self>, id: &Value, params: &Value, ctx: &Ctx) -> Result<(), RpcError> {
        let filter = params.get("notifications").cloned().unwrap_or_else(|| json!({}));
        if !filter.is_object() {
            return Err(RpcError::invalid_params("`notifications` must be an object"));
        }
        let task_ids = strings(&filter, "taskIds");
        if !task_ids.is_empty() && !ctx.has_extension(EXT_TASKS) {
            return Err(rpc::missing_extension(EXT_TASKS));
        }
        // What isn't ours, or reads through a tool this server doesn't serve, isn't acknowledged.
        let uris: Vec<String> =
            strings(&filter, "resourceSubscriptions").into_iter().filter(|u| resources::check(u, &self.toolsets).is_ok()).collect();
        // The list is of devices, which a server without `devices` doesn't say.
        let list_changed = filter["resourcesListChanged"] == true && self.toolsets.contains("devices");
        let sink = self.sink.clone();
        let acknowledge = {
            let (sink, id, uris) = (sink.clone(), id.clone(), uris.clone());
            move |agreed_tasks: &[String]| {
                let mut agreed = Map::new();
                if list_changed {
                    agreed.insert("resourcesListChanged".into(), true.into());
                }
                if !uris.is_empty() {
                    agreed.insert("resourceSubscriptions".into(), uris.into());
                }
                if !agreed_tasks.is_empty() {
                    agreed.insert("taskIds".into(), agreed_tasks.into());
                }
                sink(
                    json!({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
                    "params": {"_meta": {rpc::META_SUBSCRIPTION: id}, "notifications": agreed}})
                    .to_string(),
                );
            }
        };
        if task_ids.is_empty() {
            acknowledge(&[]);
        } else {
            self.tasks.subscribe(id.clone(), &task_ids, sink.clone(), acknowledge);
        }
        let watcher = (!uris.is_empty() || list_changed).then(|| {
            let subscription = id.clone();
            let emit: Emit = Arc::new(move |method, mut params| {
                params["_meta"] = json!({rpc::META_SUBSCRIPTION: subscription});
                sink(json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
            });
            watch::spawn(self.backend.clone(), Spec { uris: uris.clone(), list_changed }, self.timing.watch, emit)
        });
        lock(&self.listens).insert(rpc::id_key(id), Listen { subscription: id.clone(), _watcher: watcher });
        Ok(())
    }

    /// A legacy client's `resources/subscribe` and `resources/unsubscribe`.
    fn legacy_subscription(self: &Arc<Self>, method: &str, params: &Value) -> Result<Value, RpcError> {
        let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("`uri` is required"))?;
        resources::check(uri, &self.toolsets).map_err(|error| rpc::for_era(error, Era::Legacy))?;
        let mut subscribed = lock(&self.subscribed);
        if method == "resources/subscribe" {
            subscribed.uris.insert(uri.to_owned());
        } else {
            // Whichever spelling it was subscribed under, it is one resource.
            let key = resources::spelling(uri);
            subscribed.uris.retain(|u| resources::spelling(u) != key);
        }
        // An older client has no way to ask for `list_changed` (and isn't told it is offered).
        let spec = Spec { uris: subscribed.uris.iter().cloned().collect(), list_changed: false };
        if spec.uris.is_empty() {
            subscribed.watcher = None;
        } else if let Some(watcher) = &subscribed.watcher {
            watcher.update(spec);
        } else {
            let sink = self.sink.clone();
            let emit: Emit =
                Arc::new(move |method, params| sink(json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()));
            subscribed.watcher = Some(watch::spawn(self.backend.clone(), spec, self.timing.watch, emit));
        }
        Ok(json!({}))
    }
}

#[cfg(test)]
mod tests;
