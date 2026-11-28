//! Tools: one per method in `sim-api`'s table, generated from it, plus the one that opens the
//! device view. A method added to the table is a tool the next build: its description, input and
//! output schemas, annotations and toolset all come from `sim-api` (`METHODS`, `input_schema`,
//! `output_schema`, `catalog`), the same text `help` and docs/api.md show.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_api::catalog::{self, Effect, Group};
use sim_api::{METHODS, input_schema, output_schema as api_output, platforms};

use crate::app;
use crate::backend::Backend;
use crate::rpc::{Ctx, Era, RpcError};

/// The tool that opens the device view (not an API method: the view's data comes from `screen`).
pub const VIEW_TOOL: &str = "device_view";

/// Methods that can run for many seconds: a client that declares the Tasks extension gets a task
/// handle for these, and everything else a progress heartbeat while the call blocks. A method not
/// listed runs inline. (`provision` and `record` aren't API methods, so they aren't tools.)
pub const LONG_RUNNING: &[&str] = &[
    "boot",
    "boot_many",
    "create",
    "clone",
    "install",
    "settle",
    "erase",
    "shutdown",
    "export",
    "import",
    "mem_leaks",
    "mem_heap",
    "stream_start",
];

/// The environment variable that picks the toolsets: names separated by commas (`core,debug`), or
/// `all`.
pub const TOOLSETS_ENV: &str = "SIMEE_MCP_TOOLSETS";

/// The toolset a server serves when none is named: the methods of [`catalog::CORE`] and the view.
pub const CORE: &str = "core";

/// Which tools a server serves: `core`, any of `sim-api`'s groups, or all of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolsets {
    core: bool,
    groups: Vec<Group>,
    /// A narrower set this one is limited to: a client's own toolsets within the server's.
    limit: Option<Box<Toolsets>>,
}

impl Default for Toolsets {
    /// `core`: what a general agent needs to drive a device.
    fn default() -> Toolsets {
        Toolsets { core: true, groups: Vec::new(), limit: None }
    }
}

impl Toolsets {
    /// Every tool.
    pub fn all() -> Toolsets {
        Toolsets { core: true, groups: Group::ALL.to_vec(), limit: None }
    }

    /// Reads `core,debug` (names separated by commas or spaces; `all` is every one). Empty is the
    /// default. An unknown name is refused with the names there are.
    pub fn parse(text: &str) -> Result<Toolsets, String> {
        let names: Vec<&str> = text.split([',', ' ']).map(str::trim).filter(|n| !n.is_empty()).collect();
        if names.is_empty() {
            return Ok(Toolsets::default());
        }
        let mut sets = Toolsets { core: false, groups: Vec::new(), limit: None };
        let mut all = false;
        for name in names {
            match name {
                "all" => all = true,
                CORE => sets.core = true,
                _ => match Group::from_name(name) {
                    Some(g) if !sets.groups.contains(&g) => sets.groups.push(g),
                    Some(_) => {}
                    None => return Err(format!("unknown toolset `{name}` in {TOOLSETS_ENV} (choose from {})", Toolsets::names())),
                },
            }
        }
        Ok(if all { Toolsets::all() } else { sets })
    }

    /// The toolsets `SIMEE_MCP_TOOLSETS` names.
    pub fn from_env() -> Result<Toolsets, String> {
        Toolsets::parse(&std::env::var(TOOLSETS_ENV).unwrap_or_default())
    }

    /// Every toolset's name, for messages.
    pub fn names() -> String {
        std::iter::once(CORE).chain(Group::ALL.iter().map(|g| g.name())).chain(["all"]).collect::<Vec<_>>().join(", ")
    }

    /// Whether `tool` is served.
    pub fn contains(&self, tool: &str) -> bool {
        let own = (self.core && in_core(tool)) || group(tool).is_some_and(|g| self.groups.contains(&g));
        own && self.limit.as_ref().is_none_or(|l| l.contains(tool))
    }

    /// These toolsets, limited to the tools `other` serves too.
    pub fn intersect(&self, other: &Toolsets) -> Toolsets {
        let mut limited = self.clone();
        limited.limit = Some(Box::new(match limited.limit.take() {
            Some(l) => l.intersect(other),
            None => other.clone(),
        }));
        limited
    }

    /// What a client's limit is, if it has one.
    pub fn limit(&self) -> Option<&Toolsets> {
        self.limit.as_deref()
    }

    /// The names, as `SIMEE_MCP_TOOLSETS` would spell them.
    pub fn label(&self) -> String {
        if self.core && self.groups.len() == Group::ALL.len() {
            return "all".into();
        }
        let core = self.core.then_some(CORE);
        core.into_iter().chain(self.groups.iter().map(|g| g.name())).collect::<Vec<_>>().join(",")
    }
}

/// Whether `tool` is in the `core` toolset.
fn in_core(tool: &str) -> bool {
    tool == VIEW_TOOL || catalog::CORE.contains(&tool)
}

/// The group a tool belongs to (the view is an observation).
fn group(tool: &str) -> Option<Group> {
    if tool == VIEW_TOOL { Some(Group::Observe) } else { catalog::group(tool) }
}

/// What a tool does (the view only reads).
fn effect(tool: &str) -> Option<Effect> {
    if tool == VIEW_TOOL { Some(Effect::Read) } else { catalog::effect(tool) }
}

/// Parameters the API takes but a tool doesn't: `screenshot`'s `path` writes a file anywhere on the
/// host, which a client that auto-approves read-only tools must not be able to ask for. The image
/// comes back in the result instead.
const WITHHELD: &[(&str, &str, &str)] = &[
    ("screenshot", "path", "The device's screen as a PNG image, returned in the result"),
    (
        "contrast_check",
        "path",
        "Text and controls below WCAG contrast in a screenshot, from pixels alone: finds text-like regions, estimates each one's foreground and \
         background, and reports those under 4.5:1 (normal text) or 3:1 (large text of about 18 pt, icons and controls) with their box, the \
         two colours and the ratio. The screenshot with the findings boxed comes back as an image (one per appearance with appearance both, \
         which on iOS checks the light and the dark UI, then leaves the device in `restore`; Android can't switch appearance yet). Text over \
         a photo or a busy background has no one background and isn't judged. region is {x, y, width, height} in pixels; scale is pixels per \
         point (default: the device's, else 1)",
    ),
];

fn withheld(method: &str) -> Option<&'static str> {
    WITHHELD.iter().find(|(m, ..)| *m == method).map(|(_, p, _)| *p)
}

/// Tools whose results can be long lists, and where the list is (`None`: the result is the list).
/// They take `limit` and `cursor`; without either the result is the whole list, as the API's.
pub const PAGED: &[(&str, Option<&str>)] =
    &[("processes", None), ("logs", None), ("connections", None), ("devices_usage", Some("devices"))];

/// The most a page holds.
pub const MAX_LIMIT: u64 = 1000;

fn paged(tool: &str) -> Option<Option<&'static str>> {
    PAGED.iter().find(|(m, _)| *m == tool).map(|(_, field)| *field)
}

/// Whether `tool` changes nothing (the catalog's `Read`; a tool nobody classified may).
pub fn read_only(tool: &str) -> bool {
    effect(tool).is_some_and(Effect::read_only)
}

/// Whether `name` is a tool (served or not).
pub fn exists(name: &str) -> bool {
    name == VIEW_TOOL || METHODS.iter().any(|(m, ..)| *m == name)
}

/// The annotations the MCP spec defines, from what `sim-api`'s catalog says the method does. An
/// unclassified method gets only `readOnlyHint: false`, so a client applies the specification's
/// defaults (it may destroy, and isn't idempotent).
fn annotations(tool: &str) -> Value {
    let title = if tool == VIEW_TOOL { "Device view" } else { tool };
    let mut a = json!({"title": title, "openWorldHint": catalog::open_world(tool)});
    match effect(tool) {
        Some(e) if e.read_only() => a["readOnlyHint"] = true.into(),
        Some(e) => {
            a["readOnlyHint"] = false.into();
            a["destructiveHint"] = e.destructive().into();
            a["idempotentHint"] = e.idempotent().into();
        }
        None => a["readOnlyHint"] = false.into(),
    }
    a
}

/// A tool's description, for an agent: the method's own text from the table (what `help` and
/// docs/api.md say), what it does to the device, the units it takes, how slow it is, where it works
/// and its parameters.
fn describe(method: &str, params: &str, doc: &str, schema: &Value) -> String {
    let mut text = format!("{doc}{}", if doc.ends_with('.') { "" } else { "." });
    if let Some(e) = effect(method) {
        text.push(' ');
        text.push_str(e.sentence());
    }
    let pixels = schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .any(|(_, p)| p["description"].as_str().is_some_and(|d| d.contains("device pixels")));
    if pixels {
        text.push_str(" Coordinates are device pixels, the space `screen` reports (not points).");
    }
    if LONG_RUNNING.contains(&method) {
        text.push_str(" Can take a while (seconds to minutes): a client with the tasks extension gets a task to poll.");
    }
    if crate::safety::may_confirm(method) {
        text.push_str(&format!(" {}", crate::safety::confirm_sentence(method)));
    }
    if paged(method).is_some() {
        text.push_str(&format!(
            " A long list pages: give `limit` (at most {MAX_LIMIT}) and pass back `next_cursor` as `cursor` for the next page; \
             without either, the whole list."
        ));
    }
    // The view works wherever `screen` does.
    let on: Vec<&str> = platforms(if method == VIEW_TOOL { "screen" } else { method }).iter().map(|p| p.label()).collect();
    text.push_str(&format!("\n\nPlatforms: {}.", on.join(", ")));
    if !params.is_empty() {
        text.push_str(&format!(" Parameters: {params}."));
    }
    match group(method) {
        Some(g) if in_core(method) => text.push_str(&format!(" Toolsets: {CORE}, {}.", g.name())),
        Some(g) => text.push_str(&format!(" Toolset: {}.", g.name())),
        None => {}
    }
    text
}

/// A tool's input schema: the method's, less what's withheld, plus paging.
fn input(method: &str) -> Value {
    let mut schema = input_schema(method).unwrap_or_else(|| json!({"type": "object"}));
    if let (Some(name), Some(properties)) = (withheld(method), schema["properties"].as_object_mut()) {
        properties.remove(name);
    }
    if crate::safety::may_confirm(method) {
        schema["properties"]["confirm"] = json!({"type": "boolean",
            "description": "Only after the user has agreed, for a client that can't be asked itself: true confirms this call. \
                A client that can be asked is asked, and this is ignored"});
    }
    if paged(method).is_some() {
        schema["properties"]["limit"] = json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT,
            "description": "At most this many items; the result is a page with `next_cursor` (null on the last page) and `total`"});
        schema["properties"]["cursor"] = json!({"type": "string",
            "description": "A previous page's `next_cursor`: the page after it, of the same list (kept 10 minutes)"});
    }
    schema
}

/// What a tool's `structuredContent` is, before the client's revision has its say: the method's
/// result as `sim-api` describes it (nullable where the method may return nothing), with the
/// shapes the tool changes.
fn output(tool: &str) -> Value {
    let number = json!({"type": "number"});
    let size = json!({"type": "object", "properties": {"width": number, "height": number}, "required": ["width", "height"]});
    let mut schema = match tool {
        VIEW_TOOL => {
            json!({"type": "object", "properties": {"device": {"type": "string"}, "screen": size}, "required": ["device", "screen"]})
        }
        // The PNG is an image block; the structure is its size.
        "screenshot" => size,
        _ => api_output(tool).unwrap_or_else(|| json!({})),
    };
    if tool == "contrast_check" {
        // Each PNG is an image block, which `image` (its place among them) and `image_of` name.
        if let Some(properties) = schema["properties"].as_object_mut() {
            properties.remove("png");
            properties.remove("path");
            properties.insert("image".into(), json!({"type": "number"}));
            properties.insert("image_of".into(), json!({"type": "string"}));
        }
    }
    match paged(tool) {
        Some(None) => {
            let page = json!({"type": "object", "properties": {"items": schema, "next_cursor": {"type": ["string", "null"]},
                "total": number}, "required": ["items", "next_cursor", "total"]});
            json!({"anyOf": [schema, page]})
        }
        Some(Some(_)) => {
            schema["properties"]["next_cursor"] = json!({"type": ["string", "null"]});
            schema["properties"]["total"] = number;
            schema
        }
        None => schema,
    }
}

/// `schema` with a `null` result written as `{}`, which is what [`structured`] makes of one.
fn without_null(mut schema: Value) -> Value {
    if schema == json!({"type": "null"}) {
        return json!({"type": "object", "properties": {}, "additionalProperties": false});
    }
    if let Some(types) = schema["type"].as_array().filter(|t| t.contains(&json!("null"))) {
        let rest: Vec<Value> = types.iter().filter(|t| **t != "null").cloned().collect();
        if rest == [json!("object")] {
            // `{}` has none of the keys the object requires.
            schema["type"] = "object".into();
            if let Some(o) = schema.as_object_mut() {
                o.remove("required");
            }
        }
        return schema;
    }
    if let Some(alts) = schema.get_mut("anyOf").and_then(Value::as_array_mut) {
        for alt in alts.iter_mut().filter(|a| **a == json!({"type": "null"})) {
            *alt = json!({"type": "object", "maxProperties": 0});
        }
    }
    schema
}

/// `schema` as an object, as revisions before 2026-07-28 have `structuredContent`: anything else
/// is under `result`, as [`structured`] puts it.
fn as_object(schema: Value) -> Value {
    let wrap = |s: Value| {
        if s["type"] == "object" { s } else { json!({"type": "object", "properties": {"result": s}, "required": ["result"]}) }
    };
    match schema.get("anyOf").and_then(Value::as_array) {
        Some(alts) => json!({"type": "object", "anyOf": alts.iter().cloned().map(wrap).collect::<Vec<_>>()}),
        None => wrap(schema),
    }
}

/// A tool's output schema for a client of `era`.
pub fn output_schema(tool: &str, era: Era) -> Value {
    let schema = without_null(output(tool));
    if era == Era::Modern { schema } else { as_object(schema) }
}

/// One tool's definition for this client.
fn definition(method: &str, params: &str, doc: &str, ctx: &Ctx) -> Value {
    let schema = input(method);
    let (mut params, mut doc) = (params.to_string(), doc.to_string());
    if let Some((_, name, text)) = WITHHELD.iter().find(|(m, ..)| *m == method) {
        doc = (*text).to_owned();
        params = params.split(", ").filter(|t| t != name && !t.starts_with(&format!("{name}?"))).collect::<Vec<_>>().join(", ");
    }
    let mut tool = json!({
        "name": method,
        "title": method,
        "description": describe(method, &params, &doc, &schema),
        "inputSchema": schema,
        "annotations": annotations(method),
    });
    if ctx.output_schemas() {
        tool["outputSchema"] = output_schema(method, ctx.era);
    }
    tool
}

/// The tools `toolsets` serves, for this client: the method table in its order, then the view.
pub fn list(toolsets: &Toolsets, ctx: &Ctx) -> Vec<Value> {
    let mut tools: Vec<Value> = METHODS
        .iter()
        .filter(|(method, ..)| toolsets.contains(method))
        .map(|(method, params, doc)| definition(method, params, doc, ctx))
        .collect();
    if toolsets.contains(VIEW_TOOL) {
        let schema = json!({"type": "object", "properties": {"device": {"type": "string",
            "description": "A device's UDID, name or `booted` (the default)"}}});
        let doc = "Open a live view of a device: its screen, refreshed about once a second, with tap, swipe, type and button \
            controls (an MCP App; a client without app support gets the screen size)";
        let mut view = json!({
            "name": VIEW_TOOL,
            "title": "Device view",
            "description": describe(VIEW_TOOL, "", doc, &schema),
            "inputSchema": schema,
            "annotations": annotations(VIEW_TOOL),
            "_meta": {"ui": {"resourceUri": app::URI}},
        });
        if ctx.output_schemas() {
            view["outputSchema"] = output_schema(VIEW_TOOL, ctx.era);
        }
        tools.push(view);
    }
    tools
}

/// Whether `name` is a tool this server has: a method in `sim-api`'s table, or the device view.
pub fn is_tool(name: &str) -> bool {
    name == VIEW_TOOL || METHODS.iter().any(|(method, ..)| *method == name)
}

/// The tool's arguments as the method's params: a missing or null `arguments` is none, anything
/// but an object is refused.
pub fn arguments(params: &Value) -> Result<Value, RpcError> {
    match params.get("arguments") {
        None | Some(Value::Null) => Ok(json!({})),
        Some(args @ Value::Object(_)) => Ok(args.clone()),
        Some(_) => Err(RpcError::invalid_params("`arguments` must be an object")),
    }
}

/// Snapshots of long lists a client is paging through: the first page's call is the list's
/// snapshot, and each cursor names one, so the pages after it are the same list (a process list or
/// a log window doesn't shift under the client). A snapshot belongs to the client that made it
/// (one HTTP server's stateless sessions share these) and its id is a random 128-bit number, so a
/// cursor works for nobody else; serving a page asks the client's backend again whether the call
/// that made the snapshot is still allowed ([`Backend::permits`]). Kept [`Pages::TTL`]; at most
/// [`Pages::KEPT`] of them and [`Pages::MAX_BYTES`] of JSON in all (the oldest go first), and none
/// for a first page that holds the whole list.
#[derive(Default)]
pub struct Pages {
    kept: Mutex<VecDeque<Snapshot>>,
}

struct Snapshot {
    id: String,
    /// The client it was made for.
    owner: String,
    tool: String,
    /// The call that made it, as the method took it (without paging).
    args: Value,
    at: Instant,
    /// The whole result.
    value: Value,
    /// Its size as JSON text.
    bytes: usize,
}

impl Pages {
    pub const TTL: Duration = Duration::from_secs(600);
    pub const KEPT: usize = 4;
    pub const MAX_BYTES: usize = 32 << 20;

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Snapshot>> {
        self.kept.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn keep(&self, owner: &str, tool: &str, args: &Value, value: &Value) -> String {
        self.keep_within(owner, tool, args, value, Pages::KEPT, Pages::MAX_BYTES)
    }

    fn keep_within(&self, owner: &str, tool: &str, args: &Value, value: &Value, entries: usize, max_bytes: usize) -> String {
        let bytes = value.to_string().len();
        let mut kept = self.lock();
        let id = crate::tasks::new_id();
        kept.retain(|s| s.at.elapsed() < Pages::TTL);
        // Make room: the oldest go until this one fits both caps (it is kept even when alone over).
        while !kept.is_empty() && (kept.len() >= entries || kept.iter().map(|s| s.bytes).sum::<usize>() + bytes > max_bytes) {
            kept.pop_front();
        }
        kept.push_back(Snapshot {
            id: id.clone(),
            owner: owner.to_owned(),
            tool: tool.to_owned(),
            args: args.clone(),
            at: Instant::now(),
            value: value.clone(),
            bytes,
        });
        id
    }

    /// The snapshot `id` that `owner` made for `tool`: its list, and the call that made it.
    fn find(&self, owner: &str, tool: &str, id: &str) -> Option<(Value, Value)> {
        let kept = self.lock();
        let found = kept.iter().find(|s| s.id == id && s.owner == owner && s.tool == tool && s.at.elapsed() < Pages::TTL);
        found.map(|s| (s.value.clone(), s.args.clone()))
    }

    /// One page of `tool`'s list: from a new call, or from the snapshot `cursor` names.
    fn page(&self, backend: &dyn Backend, owner: &str, tool: &str, field: Option<&str>, args: &Value) -> Result<Value, String> {
        let limit = match args.get("limit") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .filter(|n| (1..=MAX_LIMIT).contains(n))
                    .ok_or_else(|| format!("`limit` must be a whole number from 1 to {MAX_LIMIT}, not {v}"))?,
            ),
        };
        let expired =
            || format!("`cursor` has expired (pages are kept 10 minutes) or isn't one of this server's: call `{tool}` again without it");
        let mut made = args.clone();
        if let Some(o) = made.as_object_mut() {
            o.remove("limit");
            o.remove("cursor");
        }
        let (value, offset, limit, id) = match args.get("cursor") {
            None | Some(Value::Null) => (backend.call(tool, &made)?, 0, limit.unwrap_or(MAX_LIMIT), None),
            Some(Value::String(cursor)) => {
                let mut parts = cursor.split('.');
                let (Some(id), Some(Ok(offset)), Some(Ok(was)), None) =
                    (parts.next(), parts.next().map(str::parse::<u64>), parts.next().map(str::parse::<u64>), parts.next())
                else {
                    return Err(expired());
                };
                if !(1..=MAX_LIMIT).contains(&was) {
                    return Err(expired());
                }
                let (value, original) = self.find(owner, tool, id).ok_or_else(expired)?;
                // The list was read when the call was allowed: ask again before showing more of it.
                backend.permits(tool, &original)?;
                (value, offset, limit.unwrap_or(was), Some(id.to_owned()))
            }
            Some(other) => return Err(format!("`cursor` is a previous page's `next_cursor`, not {other}")),
        };
        let list = match field {
            Some(f) => value.get(f),
            None => Some(&value),
        };
        let Some(items) = list.and_then(Value::as_array) else {
            // Not a list after all (an error the method reported as data): as it is.
            return Ok(value);
        };
        let total = items.len() as u64;
        let end = offset.saturating_add(limit).min(total);
        let page: Vec<Value> = items.iter().skip(offset as usize).take((end.saturating_sub(offset)) as usize).cloned().collect();
        let next = if end < total {
            let id = id.unwrap_or_else(|| self.keep(owner, tool, &made, &value));
            json!(format!("{id}.{end}.{limit}"))
        } else {
            Value::Null
        };
        Ok(match field {
            Some(f) => {
                let mut out = value.clone();
                out[f] = page.into();
                out["next_cursor"] = next;
                out["total"] = total.into();
                out
            }
            None => json!({"items": page, "next_cursor": next, "total": total}),
        })
    }
}

/// Runs one tool: the method's outcome (a failure is a tool error, not a protocol error).
/// `owner` is the client the call is for, whose snapshots its pages are.
pub fn run(backend: &dyn Backend, pages: &Pages, owner: &str, name: &str, args: &Value) -> Result<Value, String> {
    if let Some(param) = withheld(name).filter(|p| args.get(*p).is_some_and(|v| !v.is_null())) {
        return Err(format!("`{param}` isn't available through MCP: `{name}` returns the image in its result"));
    }
    if name == VIEW_TOOL {
        let device = args.get("device").and_then(Value::as_str).unwrap_or("booted");
        let screen = backend.call("screen", &json!({"device": device}))?;
        return Ok(json!({"device": device, "screen": screen}));
    }
    if let Some(field) = paged(name)
        && ["limit", "cursor"].iter().any(|k| args.get(*k).is_some_and(|v| !v.is_null()))
    {
        return pages.page(backend, owner, name, field, args);
    }
    backend.call(name, args)
}

/// A result as `structuredContent`: nothing is `{}`, an object is itself, anything else is itself
/// for the current revision and under `result` for the ones before it (which want an object).
fn structured(value: Value, era: Era) -> Value {
    match value {
        Value::Null => json!({}),
        v if v.is_object() || era == Era::Modern => v,
        v => json!({"result": v}),
    }
}

/// A method's outcome as a `CallToolResult`. A screenshot's PNG becomes an image block instead of
/// base64 in text; everything else is its JSON as text (`ok` for nothing), and as
/// `structuredContent` (what the tool's output schema describes).
pub fn result(name: &str, outcome: Result<Value, String>, era: Era) -> Value {
    let value = match outcome {
        Ok(value) => value,
        Err(message) => return json!({"content": [{"type": "text", "text": message}], "isError": true}),
    };
    if name == VIEW_TOOL {
        let (w, h) = (value["screen"]["width"].as_f64().unwrap_or(0.0), value["screen"]["height"].as_f64().unwrap_or(0.0));
        let text = format!(
            "Live view of {} ({w}×{h} px) opened; tap, swipe, type and press buttons in the view.",
            value["device"].as_str().unwrap_or("?")
        );
        return json!({"content": [{"type": "text", "text": text}], "structuredContent": value});
    }
    if let (Some(png), true) = (value.get("png").and_then(Value::as_str), name == "screenshot") {
        let meta = json!({"width": value["width"], "height": value["height"]});
        return json!({"content": [{"type": "image", "data": png, "mimeType": "image/png"},
            {"type": "text", "text": meta.to_string()}], "structuredContent": meta});
    }
    if name == "contrast_check" {
        return contrast_result(value);
    }
    let text = if value.is_null() { "ok".to_owned() } else { lifecycle_text(name, &value) };
    json!({"content": [{"type": "text", "text": text}], "structuredContent": structured(value, era)})
}

/// A result's JSON as text. A lifecycle call's `admission` and `pacing` are in `structuredContent`
/// already, so the text keeps the rest and the verdict in a few words (`ok (admit)`): the same
/// numbers twice would double what every boot costs the agent's context.
fn lifecycle_text(name: &str, value: &Value) -> String {
    let verdict = value.get("admission").and_then(|a| a["verdict"].as_str());
    let (true, Some(verdict), Some(fields)) = (name != "admission_check", verdict, value.as_object()) else { return value.to_string() };
    let mut rest = fields.clone();
    rest.remove("admission");
    rest.remove("pacing");
    if rest.is_empty() { format!("ok ({verdict})") } else { format!("{} ({verdict})", Value::Object(rest)) }
}

/// `contrast_check`'s outcome: each annotated PNG (one, or one per appearance) becomes an image block
/// and the findings stay as JSON text and `structuredContent`, so a client never reads a megabyte of
/// base64 as text.
fn contrast_result(mut value: Value) -> Value {
    let mut images = Vec::new();
    let mut lift = |v: &mut Value, label: &str| {
        if let Some(png) = v.as_object_mut().and_then(|o| o.remove("png")).and_then(|p| p.as_str().map(str::to_owned)) {
            images.push(json!({"type": "image", "data": png, "mimeType": "image/png"}));
            v["image"] = json!(images.len() - 1);
            v["image_of"] = json!(label);
        }
    };
    lift(&mut value, "screen");
    if let Some(each) = value.get_mut("appearances").and_then(Value::as_object_mut) {
        for (mode, v) in each.iter_mut() {
            lift(v, mode);
        }
    }
    let mut content = vec![json!({"type": "text", "text": value.to_string()})];
    content.extend(images);
    json!({"content": content, "structuredContent": value})
}

/// The name of the toolset that has `tool` (`all` for one no set claims).
pub fn toolset_of(tool: &str) -> &'static str {
    group(tool).map_or("all", Group::name)
}

/// The refusal of `tool`, which `toolsets` doesn't serve, naming the toolset that has it. A
/// resource that reads through a tool is refused the same way.
pub fn not_enabled(tool: &str, toolsets: &Toolsets) -> RpcError {
    let set = toolset_of(tool);
    if let Some(limit) = toolsets.limit().filter(|l| !l.contains(tool)) {
        return RpcError::invalid_params(format!(
            "Tool `{tool}` isn't allowed for this client: it is in the `{set}` toolset, and this client's policy allows `{}`. \
             The operator changes it with `toolsets` in the policy file (`simee mcp --policy`, docs/mcp-safety.md)",
            limit.label()
        ))
        .with_data(json!({"tool": tool, "toolset": set, "allowed": limit.label()}));
    }
    RpcError::invalid_params(format!(
        "Tool `{tool}` isn't enabled on this server: it is in the `{set}` toolset, and this server serves `{}`. \
         Start it with {TOOLSETS_ENV}={},{set} (or all)",
        toolsets.label(),
        toolsets.label()
    ))
    .with_data(json!({"tool": tool, "toolset": set, "enabled": toolsets.label()}))
}

/// What a tool call needs from the request to decide how to run: its name and arguments, checked.
/// A tool this server doesn't serve is refused with the toolset that has it.
pub fn parse_call(params: &Value, toolsets: &Toolsets) -> Result<(String, Value), RpcError> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
    if !exists(name) {
        return Err(RpcError::invalid_params(format!("Unknown tool: {name}")));
    }
    if !toolsets.contains(name) {
        return Err(not_enabled(name, toolsets));
    }
    let args = arguments(params)?;
    // `input: true` lets the viewer touch, type and press buttons: it needs the input tools served.
    if name == "stream_start" && args.get("input") == Some(&json!(true)) && !toolsets.contains("touch") {
        return Err(RpcError::invalid_params(format!(
            "`stream_start` with `input: true` lets viewers drive the device, which needs the `input` toolset, and this server serves `{}`. \
             Start it with {TOOLSETS_ENV}={},input (or all), or leave `input` off",
            toolsets.label(),
            toolsets.label()
        )));
    }
    Ok((name.to_owned(), args))
}

/// Whether this call should be a task for this client.
pub fn as_task(name: &str, ctx: &Ctx) -> bool {
    ctx.modern() && ctx.has_extension(crate::rpc::EXT_TASKS) && LONG_RUNNING.contains(&name)
}

#[cfg(test)]
mod tests;
