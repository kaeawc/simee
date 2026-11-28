//! What stands between an agent and the devices: confirmation of destructive calls, the policy's
//! device and read-only rules, host-health admission for boots, and the audit log.
//!
//! One [`Safety`] is shared by every [`Server`](crate::Server) in a process (a stdio client, or an
//! HTTP server's sessions). A tool call goes through it in three steps:
//!
//! 1. [`Safety::check`] looks at the call before it runs: which boots are subject to admission,
//!    whether it needs a confirmation (and says what to ask), and what the call will change.
//! 2. The call runs on a [`Safety::guard`]ed backend, which refuses what the policy forbids (a
//!    device the client may not touch, a change on a read-only server) and hides the devices it may not
//!    see. The guard is under the resources and subscriptions too, so they can't read around it.
//! 3. [`Safety::done`] records what a successful call changed (the devices this client created and
//!    booted), and [`Safety::record`] writes the audit entry.
//!
//! Confirmation is never granted by the server. A client that can be asked (MCP elicitation) is asked
//! for every call, whatever the arguments say: an agent can write `confirm: true` as easily as
//! anything else, so for such a client it counts for nothing. A client that can't be asked has to
//! pass `confirm: true`, which its user is told to leave to a person to agree to.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeSet, HashMap};
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};

use crate::audit::{self, AuditLog, Entry};
use crate::backend::Backend;
use crate::policy::{AdmissionMode, ClientPolicy, Policy};
use crate::tools::{self, VIEW_TOOL};

/// The tools every client confirms: they delete a device or data that nothing replaces (a file, an
/// app and its data, a container, the keychain, a preference), kill a process, (for `shutdown`)
/// stop a device this client didn't boot, or (`unlink`) unpair a watch, which CoreSimulator may shut
/// down and erase as it does. A policy may add tools (`confirm`).
pub const CONFIRM: &[&str] =
    &["delete", "erase", "kill", "clear_container", "reset_keychain", "rm", "uninstall", "prefs_delete", "shutdown", "unlink"];

/// The tools that confirm only for some arguments: `prefs_batch` when an op deletes a key, and
/// `console` for a command that stops the emulator or deletes a snapshot.
pub const CONFIRM_SOMETIMES: &[&str] = &["prefs_batch", "console"];

/// Whether `tool` ever asks for a confirmation of its own accord (so its input has `confirm`).
pub fn may_confirm(tool: &str) -> bool {
    CONFIRM.contains(&tool) || CONFIRM_SOMETIMES.contains(&tool)
}

/// The boots and creations that go through the host's admission (#275).
const ADMITTED: &[&str] = &["boot", "boot_many", "create", "clone"];

/// What every refusal of the policy starts with: the audit log files those as `refused`.
pub const REFUSED: &str = "refused: ";

/// The sentence a confirming tool's description carries.
pub fn confirm_sentence(tool: &str) -> String {
    let when = match tool {
        "shutdown" => "for a device this client didn't boot",
        "prefs_batch" => "when an op deletes a key",
        "console" => "for a command that stops the emulator (`kill`, `avd stop`) or deletes a snapshot, unless this client booted it",
        _ => "every time",
    };
    format!(
        "Needs the user's confirmation {when}: a client that supports elicitation asks the user; any other must pass `confirm: true`, \
         which only a person's agreement allows."
    )
}

/// A process's safety: the policy, the audit log, and what each client has made and started.
pub struct Safety {
    /// The rules of the clients the policy names, and everyone else's.
    named: HashMap<String, Arc<ClientPolicy>>,
    fallback: Arc<ClientPolicy>,
    audit: Option<AuditLog>,
    clients: Mutex<HashMap<String, Client>>,
    /// Keys the state of a confirmation to the call it was for.
    seal: RandomState,
}

/// What one client has done to the devices, which its policy and shutdowns are judged by.
#[derive(Default)]
struct Client {
    /// UDIDs of the devices it created (`create`, `clone`, `import`).
    created: BTreeSet<String>,
    /// UDIDs of the devices it booted: ones that weren't running when it asked.
    booted: BTreeSet<String>,
    /// Ids of the audio captures it started: `audio_capture_stop` names one by id, and the ids are
    /// a counter, so a client with a device list may stop only its own.
    captures: BTreeSet<String>,
}

impl Default for Safety {
    /// Confirmations and admission, no device limits, no audit log.
    fn default() -> Safety {
        Safety::new(Policy::default(), None)
    }
}

static SHARED: OnceLock<Arc<Safety>> = OnceLock::new();

impl Safety {
    pub fn new(policy: Policy, audit: Option<AuditLog>) -> Safety {
        let named = policy.client_names().map(|name| (name.to_owned(), Arc::new(policy.for_client(name)))).collect();
        Safety { named, fallback: Arc::new(policy.default.clone()), audit, clients: Mutex::default(), seal: RandomState::new() }
    }

    /// Makes `safety` what every server made without one of its own uses, for the rest of the
    /// process: `simee mcp` calls it once, before serving, so the transports needn't pass it along.
    pub fn install(safety: Safety) -> Result<(), &'static str> {
        SHARED.set(Arc::new(safety)).map_err(|_| "the MCP safety settings are already installed")
    }

    /// What a server without its own uses: what [`Safety::install`] put there, or the defaults.
    pub fn shared() -> Arc<Safety> {
        SHARED.get().cloned().unwrap_or_else(|| Arc::new(Safety::default()))
    }

    fn clients(&self) -> std::sync::MutexGuard<'_, HashMap<String, Client>> {
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The rules of the client `name`.
    pub fn policy_for(&self, name: &str) -> Arc<ClientPolicy> {
        self.named.get(name).unwrap_or(&self.fallback).clone()
    }

    /// The path of the audit log, if there is one.
    pub fn audit_path(&self) -> Option<std::path::PathBuf> {
        self.audit.as_ref().map(AuditLog::path)
    }

    /// Whether there is an audit log to write.
    pub fn audits(&self) -> bool {
        self.audit.is_some()
    }

    /// Writes one call to the audit log, if there is one.
    pub fn record(&self, entry: &Entry) {
        if let Some(log) = &self.audit {
            log.record(entry);
        }
    }

    /// A backend that does what the policy allows `client`, and no more.
    pub fn guard(self: &Arc<Safety>, backend: Arc<dyn Backend>, client: &str) -> Arc<dyn Backend> {
        let policy = self.policy_for(client);
        if !policy.read_only && policy.devices.is_none() {
            return backend;
        }
        Arc::new(Guard { inner: backend, safety: self.clone(), client: client.to_owned(), policy })
    }

    fn may_touch(&self, client: &str, policy: &ClientPolicy, device: &Resolved) -> bool {
        let clients = self.clients();
        let empty = BTreeSet::new();
        let created = clients.get(client).map_or(&empty, |c| &c.created);
        policy.allows_device(&device.udid, &device.name, created)
    }

    /// Whether `client` may touch every member of the links a link method acts on beyond the
    /// devices it names ([`linked_members`]), as the refusal that says why not.
    fn permit_links(&self, backend: &dyn Backend, client: &str, policy: &ClientPolicy, method: &str, params: &Value) -> Result<(), String> {
        for (member, _) in linked_members(backend, method, params).map_err(|e| unresolved_refusal(method, &e))? {
            if !self.may_touch(client, policy, &member) {
                return Err(device_refusal(policy, method, &member.udid, Some(&member)));
            }
        }
        Ok(())
    }

    /// Looks at a call before it runs.
    pub fn check(&self, backend: &dyn Backend, client: &str, tool: &str, args: &Value) -> Verdict {
        let policy = self.policy_for(client);
        if policy.read_only && !tools::read_only(tool) {
            return Verdict::Refuse(read_only_refusal(tool));
        }
        if tool == "console"
            && let Some(refusal) = console_refusal(args)
        {
            return Verdict::Refuse(refusal);
        }
        let mut changed: Option<Value> = None;
        if ADMITTED.contains(&tool)
            && let Err(refusal) = self.admit(&policy, tool, args, &mut changed)
        {
            return Verdict::Refuse(refusal);
        }
        let sometimes = destructive_args(tool, args);
        let asks = CONFIRM.contains(&tool) || policy.confirm_also.contains(tool) || sometimes.is_some();
        if (asks || may_confirm(tool)) && args.get("confirm").is_some() {
            let mut stripped = changed.take().unwrap_or_else(|| args.clone());
            if let Some(fields) = stripped.as_object_mut() {
                fields.remove("confirm");
            }
            changed = Some(stripped);
        }
        // Devices whose state decides what this call is (and what it leaves behind).
        let wants_state = asks || matches!(tool, "boot" | "boot_many");
        // In the call's set: a name or alias is another device (another UDID) in each set.
        let set = call_set(args);
        let resolved: Vec<(String, Result<Resolved, String>)> = if wants_state {
            device_queries(tool, args).into_iter().map(|q| (q.clone(), resolve_in(backend, &q, set))).collect()
        } else {
            Vec::new()
        };
        // Before anyone is asked to confirm, what the policy forbids is refused.
        if asks && policy.devices.is_some() {
            for (query, device) in &resolved {
                match device {
                    Ok(d) if self.may_touch(client, &policy, d) => {}
                    Ok(d) => return Verdict::Refuse(device_refusal(&policy, tool, query, Some(d))),
                    Err(e) => return Verdict::Refuse(unresolved_refusal(tool, e)),
                }
            }
            if let Err(refusal) = self.permit_links(backend, client, &policy, tool, args) {
                return Verdict::Refuse(refusal);
            }
        }
        let known: Vec<Resolved> = resolved.iter().filter_map(|(_, r)| r.as_ref().ok().cloned()).collect();
        let prepared = Prepared { args: changed, resolved: known };
        if !asks {
            return Verdict::Go(prepared);
        }
        // A shutdown of what this client started is its own to end; anything else it asks about.
        let own_to_end = tool == "shutdown" || matches!(sometimes, Some(Destructive::Console(e)) if e.ends_emulator && !e.deletes_snapshot);
        if own_to_end && !policy.confirm_also.contains(tool) {
            let clients = self.clients();
            let booted = clients.get(client).map(|c| &c.booted);
            let all_ours =
                !resolved.is_empty() && resolved.iter().all(|(_, r)| r.as_ref().is_ok_and(|d| booted.is_some_and(|b| b.contains(&d.udid))));
            if all_ours {
                return Verdict::Go(prepared);
            }
        }
        // The pair an `unlink {link}` removes, by its members' names (the id alone says nothing).
        let pair = if tool == "unlink" { linked_members(backend, tool, args).unwrap_or_default() } else { Vec::new() };
        let what = what_it_does(tool, args, &resolved, sometimes, &pair);
        let sent = prepared.args.as_ref().unwrap_or(args);
        let state = self.seal_state(client, tool, sent);
        Verdict::Confirm(Confirmation { tool: tool.to_owned(), message: format!("An agent asks to {what}."), state, what }, prepared)
    }

    /// The admission mode a boot or creation takes: the policy's unless the agent asked for a
    /// stricter or an equal one.
    fn admit(&self, policy: &ClientPolicy, tool: &str, args: &Value, changed: &mut Option<Value>) -> Result<(), String> {
        if policy.admission == AdmissionMode::Ignore {
            return Ok(());
        }
        match args.get("admission") {
            None | Some(Value::Null) => {
                let mut with = args.clone();
                if let Some(fields) = with.as_object_mut() {
                    fields.insert("admission".into(), "refuse".into());
                }
                *changed = Some(with);
                Ok(())
            }
            Some(Value::String(mode)) if mode != "ignore" => Ok(()),
            Some(_) => Err(format!(
                "{REFUSED}`{tool}` goes through the host's admission (docs/mcp-safety.md): use `admission: \"refuse\"` to fail fast with the \
                 verdict, or `\"wait\"` (`boot`, `boot_many`) to wait for room. `ignore` is the operator's to allow (`simee mcp --admission ignore`)"
            )),
        }
    }

    fn seal_state(&self, client: &str, tool: &str, args: &Value) -> String {
        format!("{:016x}", self.seal.hash_one((client, tool, args.to_string())))
    }

    /// What a call that succeeded has changed about the client's devices.
    pub fn done(&self, client: &str, tool: &str, prepared: &Prepared, args: &Value, result: &Value) {
        let mut clients = self.clients();
        let state = clients.entry(client.to_owned()).or_default();
        match tool {
            "boot" | "boot_many" => {
                state.booted.extend(prepared.resolved.iter().filter(|d| !d.running).map(|d| d.udid.clone()));
            }
            "create" | "clone" | "import" => {
                let device = if tool == "import" { &result["device"] } else { result };
                if let Some(udid) = device["udid"].as_str() {
                    state.created.insert(udid.to_owned());
                }
            }
            "audio_capture_start" => {
                if let Some(id) = result["capture_id"].as_str() {
                    state.captures.insert(id.to_owned());
                }
            }
            "audio_capture_stop" => {
                if let Some(id) = args["capture_id"].as_str() {
                    state.captures.remove(id);
                }
            }
            "delete" => {
                for d in &prepared.resolved {
                    state.created.remove(&d.udid);
                    state.booted.remove(&d.udid);
                }
            }
            "shutdown" => {
                for d in &prepared.resolved {
                    state.booted.remove(&d.udid);
                }
            }
            "console" if matches!(destructive_args(tool, args), Some(Destructive::Console(e)) if e.ends_emulator) => {
                for d in &prepared.resolved {
                    state.booted.remove(&d.udid);
                }
            }
            _ => {}
        }
    }
}

/// What [`Safety::check`] says about a call.
pub enum Verdict {
    /// Run it.
    Go(Prepared),
    /// Don't: the message says why and what the client can do.
    Refuse(String),
    /// Only with a confirmation: ask for it, then run the call as prepared.
    Confirm(Confirmation, Prepared),
}

/// A call as it will run.
#[derive(Default)]
pub struct Prepared {
    /// The arguments the call runs with if they aren't the client's (admission added, `confirm`
    /// removed).
    pub args: Option<Value>,
    /// The devices the call names, as they were before it ran.
    resolved: Vec<Resolved>,
}

impl Prepared {
    pub fn args<'a>(&'a self, sent: &'a Value) -> &'a Value {
        self.args.as_ref().unwrap_or(sent)
    }
}

/// What a client is asked to agree to.
pub struct Confirmation {
    tool: String,
    /// What the user reads.
    pub message: String,
    /// What is being agreed to, as a clause (`erase iPhone 17 (UDID)`).
    what: String,
    /// Ties an answer to the call it was for (`requestState`, in the current revision).
    state: String,
}

/// The key of the confirmation in `inputRequests`, and the field in its form.
const ASK: &str = "confirm";

/// What a person said.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No(String),
}

impl Confirmation {
    /// The `elicitation/create` parameters (form mode): one yes/no, defaulting to no.
    pub fn elicit_params(&self) -> Value {
        json!({"mode": "form",
            "message": format!("{} This can't be undone, except what is only stopped. Allow it?", self.message),
            "requestedSchema": {"type": "object",
                "properties": {ASK: {"type": "boolean", "title": "Allow", "description": self.what, "default": false}},
                "required": [ASK]}})
    }

    /// The current revision's way to ask: a result that says it needs input, which the client
    /// answers by calling again with `inputResponses` and the same `requestState`.
    pub fn input_required(&self) -> Value {
        json!({"resultType": "input_required",
            "inputRequests": {ASK: {"method": "elicitation/create", "params": self.elicit_params()}},
            "requestState": self.state})
    }

    /// What the client's retry says: `None` when it doesn't answer this call's question.
    pub fn answered_in(&self, params: &Value) -> Option<Answer> {
        if params.get("requestState").and_then(Value::as_str) != Some(self.state.as_str()) {
            return None;
        }
        params.get("inputResponses").and_then(|r| r.get(ASK)).map(Confirmation::read)
    }

    /// An `ElicitResult` as an answer: yes only for an accepted form that says yes.
    pub fn read(result: &Value) -> Answer {
        match result.get("action").and_then(Value::as_str) {
            Some("accept") if result["content"][ASK] == true => Answer::Yes,
            Some("accept") => Answer::No("the user accepted the form without allowing it".into()),
            Some("decline") => Answer::No("the user declined".into()),
            Some("cancel") => Answer::No("the user dismissed the question".into()),
            _ => Answer::No("the client sent no usable answer".into()),
        }
    }

    /// The refusal for a client that wasn't asked.
    pub fn needs_argument(&self, why: &str) -> String {
        format!(
            "{REFUSED}`{tool}` needs the user's confirmation: {what}. {why} Ask the user, and only if they agree call `{tool}` again \
             with `confirm: true` (nothing was done).",
            tool = self.tool,
            what = self.what,
        )
    }

    /// The refusal for a client whose user said no.
    pub fn declined(&self, reason: &str) -> String {
        format!("{REFUSED}`{}` was not confirmed: {reason} ({}). Nothing was done.", self.tool, self.what)
    }
}

/// A device a call names, as it was when the call arrived.
#[derive(Clone, Debug, PartialEq)]
struct Resolved {
    udid: String,
    name: String,
    running: bool,
}

/// The device set a call acts in (iOS methods take `set?`); `None` is the default set.
fn call_set(params: &Value) -> Option<&Value> {
    params.get("set").filter(|s| !s.is_null())
}

/// The device `query` names (a UDID, a name, `booted`) in the device set `set`, by the API's own
/// lookup (a device has a different UDID in each set); `None` is the default lookup.
fn resolve_in(backend: &dyn Backend, query: &str, set: Option<&Value>) -> Result<Resolved, String> {
    let mut params = json!({"device": query});
    if let Some(set) = set {
        params["set"] = set.clone();
    }
    let info = backend.call("info", &params)?;
    let device = &info["device"];
    let udid = device["udid"].as_str().ok_or_else(|| format!("`info` doesn't say which device `{query}` is"))?;
    let state = device["state"].as_str().unwrap_or("");
    Ok(Resolved {
        udid: udid.to_owned(),
        name: device["name"].as_str().unwrap_or(udid).to_owned(),
        running: state.eq_ignore_ascii_case("booted") || state.eq_ignore_ascii_case("booting"),
    })
}

/// Whether a tool's input has a `device` that names a device to act on, and whether the call
/// needs it. (`create`'s `device` is the hardware profile of a new AVD, not a device.)
fn device_param(tool: &str) -> Option<bool> {
    static TOOLS: OnceLock<HashMap<&'static str, bool>> = OnceLock::new();
    let tools = TOOLS.get_or_init(|| {
        sim_api::METHODS
            .iter()
            .filter(|(method, ..)| *method != "create")
            .filter_map(|(method, params, _)| {
                let first = params.split([',', ';']).map(str::trim).find(|p| p.trim_end_matches('?') == "device")?;
                Some((*method, !first.ends_with('?')))
            })
            .collect()
    });
    if tool == VIEW_TOOL { Some(false) } else { tools.get(tool).copied() }
}

/// The devices a call names, as the client wrote them. A tool that needs a `device` and gets none
/// means the booted one; one that only filters by it (`streams`) means all, which names none.
fn device_queries(tool: &str, args: &Value) -> Vec<String> {
    if matches!(tool, "boot_many" | "link") {
        return args.get("devices").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    }
    let Some(required) = device_param(tool) else { return Vec::new() };
    match args.get("device") {
        None | Some(Value::Null) if tool == VIEW_TOOL || required => vec!["booted".to_owned()],
        Some(Value::String(q)) => vec![q.clone()],
        // Not a string, or a listing of everything: the method's own answer.
        _ => Vec::new(),
    }
}

/// Every device a call names for the policy to judge: its `device`s, and the devices whose screens
/// it asks to show on a camera (`camera_link`'s `source`, `boot`'s `camera_back` and
/// `camera_front`, as `device:<name>`). Linking a forbidden device's screen to an allowed device's
/// camera hands the allowed device the forbidden one's screen.
fn named_devices(tool: &str, args: &Value) -> Vec<String> {
    let mut named = device_queries(tool, args);
    let sources: &[&str] = match tool {
        "camera_link" => &["source"],
        "boot" | "boot_many" => &["camera_back", "camera_front"],
        _ => &[],
    };
    for key in sources {
        if let Some(device) = args.get(*key).and_then(Value::as_str).and_then(sim_api::source_device) {
            named.push(device);
        }
    }
    named
}

/// The members of the links a link method acts on that the call doesn't name: the link `link` (an
/// id) names, and for `unlink` and `link_status` by `device`, every link of that device (an `unlink`
/// removes them all, and a status shows the other member). They are read from `links` in the call's
/// set, since an id says no device. An id that isn't among them can't be checked, so it is an error.
fn linked_members(backend: &dyn Backend, method: &str, params: &Value) -> Result<Vec<(Resolved, String)>, String> {
    if !matches!(method, "link" | "unlink" | "link_status") {
        return Ok(Vec::new());
    }
    let id = params.get("link").and_then(Value::as_str);
    let device = params.get("device").and_then(Value::as_str).filter(|_| method != "link");
    if id.is_none() && device.is_none() {
        return Ok(Vec::new());
    }
    let mut query = json!({});
    let set = call_set(params);
    if let Some(set) = set {
        query["set"] = set.clone();
    }
    let listed = backend.call("links", &query)?;
    let rows = listed.as_array().map(Vec::as_slice).unwrap_or_default();
    let member_ids = |row: &Value| -> Vec<String> {
        row["members"].as_array().into_iter().flatten().filter_map(|m| m["device"].as_str().map(str::to_owned)).collect()
    };
    let touched: Vec<&Value> = match (id, device) {
        (Some(id), _) => {
            let found: Vec<&Value> = rows.iter().filter(|r| r["id"].as_str().is_some_and(|i| i.eq_ignore_ascii_case(id))).collect();
            if found.is_empty() {
                return Err(format!("no link `{id}` to check its devices against"));
            }
            found
        }
        (None, Some(query)) => {
            // In the call's set, where the links were read: a device has another UDID in each set.
            let udid = resolve_in(backend, query, set)?.udid;
            rows.iter().filter(|r| member_ids(r).contains(&udid)).collect()
        }
        (None, None) => Vec::new(),
    };
    Ok(touched
        .into_iter()
        .flat_map(|row| row["members"].as_array().cloned().unwrap_or_default())
        .filter_map(|m| {
            let udid = m["device"].as_str()?.to_owned();
            let name = m["name"].as_str().map_or_else(|| udid.clone(), str::to_owned);
            let role = m["role"].as_str().unwrap_or("device").to_owned();
            Some((Resolved { udid, name, running: m["running"] == true }, role))
        })
        .collect())
}

/// `tool`'s effect on its devices, as a clause the user reads: `erase iPhone 17 (UDID)`.
fn what_it_does(
    tool: &str,
    args: &Value,
    targets: &[(String, Result<Resolved, String>)],
    sometimes: Option<Destructive>,
    pair: &[(Resolved, String)],
) -> String {
    let label = |q: &str, r: &Result<Resolved, String>| match r {
        Ok(d) if d.name != d.udid => format!("`{}` ({})", d.name, d.udid),
        Ok(d) => format!("`{}`", d.udid),
        Err(_) => format!("`{q}`"),
    };
    let on = targets.iter().map(|(q, r)| label(q, r)).collect::<Vec<_>>().join(", ");
    let on = if on.is_empty() { "the device".to_owned() } else { on };
    let text = |key: &str| args.get(key).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).unwrap_or_else(|| "?".into());
    match tool {
        "delete" => format!("delete the device {on} and everything on it"),
        "erase" => format!("erase {on}: its content and settings are deleted"),
        "kill" => format!(
            "send signal {} to process {} on {on}",
            args.get("signal").map_or_else(|| "KILL".into(), |s| s.to_string()),
            text("pid")
        ),
        "clear_container" => format!("empty `{}` on {on}: an app's data is deleted", text("path")),
        "reset_keychain" => format!("delete every keychain item on {on}"),
        "shutdown" => format!("shut down {on}, which this client didn't boot"),
        "unlink" => match args.get("link").and_then(Value::as_str) {
            Some(id) => {
                let members = pair.iter().map(|(d, role)| format!("{role} `{}`", d.name)).collect::<Vec<_>>().join(" and ");
                let named = if members.is_empty() { format!("`{id}`") } else { format!("`{id}` ({members})") };
                format!("unlink {named}: CoreSimulator may shut its watch down and erase it")
            }
            None => format!("unlink every link of {on}: CoreSimulator may shut each watch down and erase it"),
        },
        "rm" => format!("remove `{}` from the data of {on}: files are deleted", text("path")),
        "uninstall" => format!("uninstall `{}` from {on}, with its data", text("bundle_id")),
        "prefs_delete" => format!("delete the preference `{}` in `{}` of `{}` on {on}", text("key"), text("store"), text("bundle_id")),
        "prefs_batch" => format!("delete preference keys of `{}` in `{}` on {on}", text("bundle_id"), text("store")),
        "console" => {
            let commands = console_commands(args).join("; ").replace(['\r', '\n'], " ");
            match sometimes {
                Some(Destructive::Console(e)) if e.ends_emulator && e.deletes_snapshot => {
                    format!("send `{commands}` to the emulator console of {on}: the emulator stops and a snapshot is deleted")
                }
                Some(Destructive::Console(e)) if e.deletes_snapshot => {
                    format!("send `{commands}` to the emulator console of {on}: a snapshot is deleted")
                }
                _ => format!("send `{commands}` to the emulator console of {on}: the emulator stops"),
            }
        }
        other => format!("run `{other}` on {on}"),
    }
}

/// The longest console command, in bytes, that is certainly one line: the stock console starts a
/// new line after 4095 bytes.
const CONSOLE_LINE_LIMIT: usize = 4095;

/// Why a `console` call is refused outright: a command with a control character, or one that is a
/// whole line or longer. The stock console drops a CR instead of ending a line on it, ends a command
/// at a NUL and starts a line after 4095 bytes, and `emu`'s ends one at an LF, so `ki\rll`,
/// `kill\0x` and 4095 bytes before `kill` each run a `kill` that no reading of the text by the
/// first word, or by a split at line breaks, finds. No model of either parser decides it: a command
/// that could be read two ways isn't run.
fn console_refusal(args: &Value) -> Option<String> {
    let reason = console_commands(args).into_iter().find_map(|command| {
        if command.chars().any(char::is_control) {
            Some("has a control character (a line break, CR, NUL, tab: the console reads those differently from this check)".to_owned())
        } else if command.len() >= CONSOLE_LINE_LIMIT {
            Some(format!("is {} bytes, and the console starts a new line after {} bytes of one", command.len(), CONSOLE_LINE_LIMIT))
        } else {
            None
        }
    })?;
    Some(format!(
        "{REFUSED}a `console` command {reason}, so what it would do can't be told from here. Send one command per entry of `commands`, \
         each a single short line of plain text (nothing was sent)"
    ))
}

/// Why a call to a tool that only sometimes confirms does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Destructive {
    /// What the commands of a `console` call do between them.
    Console(sim_core::console::Effects),
    /// A batch of preferences with a delete in it.
    Deletes,
}

/// The commands a `console` call sends.
fn console_commands(args: &Value) -> Vec<&str> {
    match (args.get("command").and_then(Value::as_str), args.get("commands").and_then(Value::as_array)) {
        (Some(c), _) => vec![c],
        (None, Some(list)) => list.iter().filter_map(Value::as_str).collect(),
        (None, None) => Vec::new(),
    }
}

/// Whether this call to `tool` destroys something though the tool doesn't always: a `console`
/// that ends the emulator or deletes a snapshot does what `shutdown` and a delete do behind
/// another tool's name, and a `prefs_batch` with a delete op is a `prefs_delete`. The console runs
/// every line of every command, so each is read ([`sim_core::console`], which also holds the names
/// `emu`'s console answers to).
fn destructive_args(tool: &str, args: &Value) -> Option<Destructive> {
    match tool {
        "console" => Some(sim_core::console::effects_of(console_commands(args))).filter(|e| e.any()).map(Destructive::Console),
        "prefs_batch" => {
            let ops = args.get("ops").and_then(Value::as_array)?;
            // An op that isn't an object is the method's to refuse; one that says `delete` at all is asked about.
            ops.iter().any(|op| op.get("delete").is_some_and(|d| d != &json!(false))).then_some(Destructive::Deletes)
        }
        _ => None,
    }
}

fn read_only_refusal(tool: &str) -> String {
    format!(
        "{REFUSED}this server is read-only for this client, and `{tool}` changes a device or the host. The operator allows changes by \
         starting `simee mcp` without `--read-only` (or with `read_only: false` for this client in the policy file)"
    )
}

fn device_refusal(policy: &ClientPolicy, tool: &str, query: &str, device: Option<&Resolved>) -> String {
    let which = device.map_or_else(|| format!("`{query}`"), |d| format!("`{}` ({})", d.name, d.udid));
    let allowed = policy.devices.as_deref().map(|d| d.join(", ")).filter(|d| !d.is_empty()).unwrap_or_else(|| "no device".into());
    format!(
        "{REFUSED}`{tool}` names {which}, which this client may not touch (its policy allows {allowed}). The operator widens it with \
         `--allow-device` or the policy file (docs/mcp-safety.md)"
    )
}

fn unresolved_refusal(tool: &str, why: &str) -> String {
    format!("{REFUSED}`{tool}` can't be checked against this client's device policy: {why}")
}

/// The backend the policy shows a client: it refuses what the client may not do and leaves out
/// the devices it may not touch.
struct Guard {
    inner: Arc<dyn Backend>,
    safety: Arc<Safety>,
    client: String,
    policy: Arc<ClientPolicy>,
}

impl Guard {
    /// Whether a listed device, whose id is under `id`, is one this client may see.
    fn visible(&self, row: &Value, id: &str) -> bool {
        let text = |key: &str| row[key].as_str().unwrap_or("").to_owned();
        self.safety.may_touch(&self.client, &self.policy, &Resolved { udid: text(id), name: text("name"), running: false })
    }
}

impl Guard {
    /// Whether this client may touch the device `query` names in the call's device set `set`, as
    /// the error that says why not.
    fn permit(&self, method: &str, query: &str, set: Option<&Value>) -> Result<(), String> {
        let device = resolve_in(&*self.inner, query, set).map_err(|e| unresolved_refusal(method, &e))?;
        if self.safety.may_touch(&self.client, &self.policy, &device) {
            Ok(())
        } else {
            Err(device_refusal(&self.policy, method, query, Some(&device)))
        }
    }
}

impl Guard {
    /// What the policy says about a call, before it runs: the read-only rule and, for a client
    /// limited to some devices, every device the call names.
    fn authorize(&self, method: &str, params: &Value) -> Result<(), String> {
        if self.policy.read_only && !tools::read_only(method) {
            return Err(read_only_refusal(method));
        }
        if self.policy.devices.is_none() {
            return Ok(());
        }
        let mut queries = named_devices(method, params);
        if method == "clone" || method == "create" || method == "import" {
            // A device made here is the client's to touch only if its name allows or created counts.
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if !self.policy.allows_new_name(name) {
                return Err(format!(
                    "{REFUSED}`{method}` would make `{name}`, which this client couldn't touch afterwards (its policy allows {}). Pick a name it allows",
                    self.policy.devices.as_deref().map(|d| d.join(", ")).unwrap_or_default()
                ));
            }
        }
        queries.sort();
        queries.dedup();
        if matches!(method, "streams" | "camera_links") && params.get("device").is_none_or(Value::is_null) {
            return Err(format!(
                "{REFUSED}`{method}` without `device` lists every device's, and this client may touch only some: name a device"
            ));
        }
        let set = call_set(params);
        for query in queries {
            self.permit(method, &query, set)?;
        }
        // A link is named by its id, and an unlink by device removes every link of it: each member
        // of each is judged.
        self.safety.permit_links(&*self.inner, &self.client, &self.policy, method, params)?;
        // A capture is named by its id, which says no device: it is the client's own to stop or not.
        if method == "audio_capture_stop" {
            let id = params.get("capture_id").and_then(Value::as_str).unwrap_or("");
            if !self.safety.clients().get(&self.client).is_some_and(|c| c.captures.contains(id)) {
                return Err(format!(
                    "{REFUSED}`{method}` names a capture this client didn't start, and a client with a device list may stop only its own"
                ));
            }
        }
        // A stream is named by its id, and the device it shows isn't in the call.
        if matches!(method, "stream_status" | "stream_stop")
            && let Some(id) = params.get("stream_id").and_then(Value::as_str)
        {
            let streams = self.inner.call("streams", &json!({}))?;
            let owner = streams.as_array().into_iter().flatten().find(|s| s["stream_id"] == id).and_then(|s| s["device"].as_str());
            if let Some(device) = owner {
                self.permit(method, device, set)?;
            }
        }
        Ok(())
    }
}

impl Backend for Guard {
    fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
        self.authorize(method, params)?;
        let restricted = self.policy.devices.is_some();
        let mut result = self.inner.call(method, params)?;
        if restricted {
            match method {
                "devices" => {
                    if let Some(rows) = result.as_array_mut() {
                        rows.retain(|r| self.visible(r, "udid"));
                    }
                }
                "devices_usage" => {
                    if let Some(rows) = result.get_mut("devices").and_then(Value::as_array_mut) {
                        rows.retain(|r| self.visible(r, "id"));
                    }
                }
                // A link shows both its members: one the client may not see hides it.
                "links" => {
                    if let Some(rows) = result.as_array_mut() {
                        rows.retain(|r| r["members"].as_array().into_iter().flatten().all(|m| self.visible(m, "device")));
                    }
                }
                _ => {}
            }
        }
        Ok(result)
    }

    fn permits(&self, method: &str, params: &Value) -> Result<(), String> {
        self.authorize(method, params)
    }
}

/// How a call that ran is filed in the audit log.
pub fn outcome(ran: &Result<Value, String>) -> (&'static str, Option<String>, Option<String>) {
    match ran {
        Ok(value) => (audit::status::OK, None, value["admission"]["verdict"].as_str().map(str::to_owned)),
        Err(message) if message.starts_with(REFUSED) || message.starts_with("admission ") => {
            (audit::status::REFUSED, Some(message.clone()), None)
        }
        Err(message) => (audit::status::ERROR, Some(message.clone()), None),
    }
}

#[cfg(test)]
mod tests;
