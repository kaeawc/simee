//! What each method returns, as a JSON Schema, for front ends that describe a method's result (the
//! MCP server's output schemas).
//!
//! [`OUTPUTS`] writes each result in a short type grammar, the way [`METHODS`](crate::METHODS)
//! writes parameters:
//!
//! ```text
//! null | string | number | boolean | object | any     a JSON type (object: any object; any: anything)
//! "a"|"b"                                              one of these strings
//! [T]                                                  an array of T
//! {name: T, other?: T, "1s": T}                        an object; `?` keys may be missing
//! {*: T}                                               an object of T, keys unknown
//! {...@name, more: T}                                  the fields of a shared part, and more
//! @name                                                a shared type from DEFS
//! A | B                                                either
//! ```
//!
//! The objects are open (a result may grow a key before its schema does), and a key is required
//! only where every platform always sets it. A method's result that differs by platform is the
//! union of them (`rotate`: `{orientation}` on iOS, `null` on Android).

use serde_json::{Map, Value, json};

use crate::METHODS;

/// Shared types, by name.
const DEFS: &[(&str, &str)] = &[
    ("device", "{...@device_fields}"),
    (
        "device_fields",
        r#"udid: string, name: string, state: string, runtime_id: string, device_type_id: string, available: boolean,
            platform: "ios"|"android", form_factor: string|null, sub_platform: string|null"#,
    ),
    (
        "admission",
        r#"{verdict: "admit"|"hold"|"backoff", reasons: [string], predicted: {subject: string, devices: number, admissible: number,
            costs: [object], boot_cpu_pct: number, idle_cpu_pct: number, boot_memory_bytes: number, idle_memory_bytes: number,
            memory_used_pct: object|null, memory_available_bytes: object|null, load: object|null}, host_basis?: string}"#,
    ),
    ("pacing", "{waited_s: number, timed_out: boolean, cancelled: boolean, steps: [object]}"),
    ("rate", r#"{"1s": number|null, "1m": number|null}"#),
    (
        "usage",
        "{kind?: \"simulator\"|\"emu\"|\"emulator\", id: string, pid?: number, pids: number, cpu_pct: @rate, resident_bytes: number|null,
            footprint_bytes: number|null, disk_read_bytes_per_s: @rate, disk_write_bytes_per_s: @rate, energy_w: @rate|null,
            name?: string, platform?: string, form_factor?: string|null, sub_platform?: string|null}",
    ),
    (
        "profile",
        r#"{platform: string, form_factor: string, sub_platform: string|null, width: number, height: number,
            density: {unit: "dpi"|"scale", value: number}, shape: string,
            fold: {angle: number, folded_width: number, folded_height: number}|null, inputs: [string]}"#,
    ),
    ("store", "{id: string, kind: string, name: string, bundle_id: string, platform: string}"),
    (
        "camera_link",
        r#"{device: string, platform: string, facing: "back"|"front", source: string, name?: string, live?: string|null,
            state: "linked"|"saved"|"relinked"|"unsaved"|"off"|"source missing"|"permission missing", fix?: string}"#,
    ),
    ("prefs_change", r#"store: string, visible: "now"|"after_relaunch", app: "untouched"|"stopped"|"relaunched", note?: string"#),
    ("frame", "{address: string, symbol: string, image?: string, source?: string}"),
    (
        "mem",
        "pid: number, process?: string, identifier?: string, path?: string, physical_footprint_bytes?: number|null,
            physical_footprint_peak_bytes?: number|null, warnings?: [string], raw?: string",
    ),
    (
        "shape",
        "{offline: boolean, latency_ms: number, down_kbps: number, up_kbps: number, loss_pct: number, port_base: number,
            port_count: number, rules: [{pattern: string, offline: boolean|null, latency_ms: number|null, down_kbps: number|null,
            up_kbps: number|null, loss_pct: number|null}]}",
    ),
    (
        "stream",
        r#"stream_id: string, device: string, name: string, platform: string, whip_url: string, whip_resource: string|null,
            state: "starting"|"running"|"ended", reason?: string, input: boolean, input_note?: string, lease_ttl_s: number, lease_expires_in_s: number,
            stages: [{stage: string, ms: number}], stats: {encoded: number, sent: number, heartbeats: number, keyframes: number,
            bytes: number, fps: number|null, bitrate_bps: number|null, bitrate_estimate_bps: number|null, pli: number, fir: number,
            input_events: number, input_errors: number, last_input_error?: string}"#,
    ),
    (
        "finding",
        r#"{x: number, y: number, width: number, height: number, kind: "text"|"large_text"|"component", foreground: string,
            background: string, ratio: number, required: number}"#,
    ),
    (
        "load",
        "cpu_s?: number, background_cpu_s?: number, procs?: number, cpu_pct?: number|null, background_cpu_pct?: number|null,
            foreground_cpu_pct?: number|null",
    ),
    ("reading", "{property: string, id: string, area_id: number, status: string, value: any}"),
];

/// (method, what a successful call returns). `null` is a call with nothing to report.
pub const OUTPUTS: &[(&str, &str)] = &[
    ("help", "[{method: string, params: string, doc: string, platforms: [string]}]"),
    ("identity", "{build: string, exe: string|null, env: string}"),
    (
        "host",
        r#"{snapshot: object, averages: [object], load_trend: string, thermal_state: string|null, thresholds: object,
            verdict: {verdict: "admit"|"hold"|"backoff", reason: string, reasons: [string], load_limit: number|null}, reasons: [string], history_note?: string}"#,
    ),
    (
        "devices_usage",
        "{available: boolean, reason?: string, samples: number, window_s: @rate, devices: [@usage], simee: @usage, host: object}",
    ),
    (
        "form_factors",
        "[{form_factor: string, platform: string, supported: boolean, sub_platform?: string|null, label?: string,
            inputs?: [{input: string, label: string}], profile?: @profile, reason?: string}]",
    ),
    ("devices", "[@device]"),
    ("runtimes", "[{identifier: string, name: string, version: string, build: string, available: boolean, platform: string}]"),
    (
        "device_types",
        "[{identifier: string, name: string, model: string, product_family: string, screen_width: number, screen_height: number,
            screen_scale: number, platform: string, form_factor: string|null}]",
    ),
    ("create", "{...@device_fields, admission?: @admission}"),
    ("delete", "null"),
    (
        "boot",
        "{device?: string, boot_s?: number, was_booted?: boolean, profile?: object, settled?: boolean, settle_s?: number,
            settle_error?: string, waited_s?: number, ...@load, admission?: @admission, pacing?: @pacing}",
    ),
    (
        "boot_many",
        "{concurrency: number, all_s: number, failed: number, unsettled: number, cancelled?: boolean,
            devices: [{device: string, error?: string, boot_s?: number, started_s?: number, done_s?: number, console_port?: number,
            serial?: string, settled?: boolean, settle_s?: number, settle_error?: string}], admission?: @admission, pacing?: @pacing}",
    ),
    ("admission_check", "@admission"),
    ("settle", "{settle_s: number, ...@load}"),
    ("profile", "{name: string, failed: [string], disabled?: number, already?: number, not_loaded?: [string], enabled?: number}"),
    ("clone", "{...@device_fields, admission?: @admission}"),
    (
        "export",
        "{device: string, to: string, summary: object, identity: object, captures: {disks: boolean, ram: boolean}, resume: string,
            secrets: [object], guest_identity: object, dropped: [string]}",
    ),
    (
        "import",
        "{device: @device, system_image: string, mismatch: string|null, dropped: [string], extracted: object, exported_as: string,
            guest_identity?: object, note?: string, resume?: string}",
    ),
    ("shutdown", "null"),
    ("erase", "null"),
    ("screen", "{width: number, height: number}"),
    ("screenshot", "{width: number, height: number, png?: string, path?: string}"),
    (
        "stream_start",
        "{stream_id: string, whip_resource: string, lease_ttl_s: number, device: string, input?: boolean, input_note?: string}",
    ),
    ("stream_status", "{...@stream}"),
    ("stream_stop", "{...@stream, deleted: boolean, delete_error?: string}"),
    ("streams", "[{...@stream}]"),
    (
        "contrast_check",
        r#"{failing: number, scale: number, scale_source: "param"|"device"|"assumed", width?: number, height?: number, png?: string,
            path?: string, checked?: number, findings?: [@finding], appearances?: {*: object}, restored?: "light"|"dark"}"#,
    ),
    ("tap", "null"),
    ("swipe", "null"),
    ("touch", "null"),
    ("button", "null"),
    ("type", "null"),
    ("key", "null"),
    ("open_url", "null"),
    ("install", "null"),
    ("uninstall", "null"),
    ("launch", "{pid: number|null, activity?: string}"),
    ("terminate", "null"),
    ("apps", "[string]"),
    ("appearance", "null"),
    ("location", "null"),
    ("privacy", "null"),
    ("push", "null"),
    ("status_bar", "null"),
    ("clear_status_bar", "null"),
    ("content_size", "null"),
    ("increase_contrast", "null"),
    ("dynamic_island", "null"),
    ("backlight", "null"),
    ("keyboard_language", "null"),
    ("hardware_keyboard", "null"),
    ("ca_debug", "null"),
    ("ca_debug_state", "[{id: number, option: string, label: string, on: any}]"),
    ("reset_keychain", "null"),
    ("memory_warning", "null"),
    ("notify", "null"),
    ("biometric", "null"),
    ("exec", "{pid: number|null, stdout: string, stderr: string, exit_code?: number|null, truncated?: boolean}"),
    ("logs", "[string]"),
    (
        "processes",
        "[{pid: number, ppid: number, cpu: number, rss_kb: number, elapsed: string, name: string, path: string, package?: string|null}]",
    ),
    ("kill", "null"),
    ("connections", "[{pid: number|null, process: string, protocol: string, local: string, remote: string, state: string, uid?: number}]"),
    ("traffic", "[{pid: number|null, process: string, bytes_in: number, bytes_out: number, uid?: number}]"),
    (
        "mem_leaks",
        "{...@mem, leak_count: number, stacks: boolean, leaks: [object], nodes?: number|null, malloced_bytes?: number|null,
            leaked_bytes?: number|null, note?: string}",
    ),
    ("mem_heap", "{...@mem, classes: [object], zones?: number|null, nodes?: number|null, bytes?: number|null}"),
    (
        "mem_vmmap",
        "{...@mem, regions: [object], total: object|null, readonly_libraries?: object, writable_regions?: object,
            malloc_zones?: [object], malloc_total?: object|null}",
    ),
    (
        "mem_history",
        "{...@mem, events: [{event: string, start: string, end: string, size: number|null, stack: [@frame]}], address: string}",
    ),
    ("containers", "[{bundle_id: string, data: string|null, bundle: string|null, data_bytes: number, bundle_bytes: number}]"),
    ("ls", "[{name: string, dir: boolean, bytes: number}]"),
    ("read", "{bytes: number, base64: string, text: string|null}"),
    ("write", "null"),
    ("rm", "null"),
    ("du", "{bytes: number}"),
    ("clear_container", "null"),
    ("defaults_read", "string"),
    ("defaults_write", "null"),
    ("prefs_stores", "[@store]"),
    ("prefs_read", "[{store: @store, values?: {*: object}, error?: string}]"),
    ("prefs_write", "{...@prefs_change, key: string}"),
    ("prefs_delete", "{...@prefs_change, key: string}"),
    ("prefs_batch", "{...@prefs_change, keys: [string]}"),
    (
        "prefs_watch",
        "{cursor: string, events: [{store: string, key: string, old: object|null, new: object|null, at: string}], ended?: boolean,
            reset?: boolean, missed?: number}",
    ),
    ("crash_reports", "[string]"),
    (
        "info",
        "{device: @device, data_path?: string, runtime_root?: string|null, avd_dir?: string|null, system_image?: string|null,
            profile?: @profile|null, running?: object|null}",
    ),
    (
        "network_shape",
        r#"{profile: @shape, proxy_port: number|null, injected: boolean, inject_mode: "none"|"launchd"|"boot", profile_path?: string,
            commands?: [string]}"#,
    ),
    ("network_state", "{profile: @shape, proxy_port: number|null, injected: boolean, boot_injected: boolean, network_status?: [string]}"),
    (
        "network_flows",
        "[{started: number, method: string, host: string, port: number, outcome: string, bytes_up: number, bytes_down: number,
            millis: number}]",
    ),
    ("network_clear", "null"),
    ("audio_state", "{settings: object|null, muted: boolean, host_devices: [object]|null}"),
    ("audio_volume", "null"),
    ("audio_route", "null"),
    ("audio_capture_start", "{capture_id: string, started: object}"),
    (
        "audio_capture_stop",
        "{frames?: number, sample_rate?: number, duration_ms?: number, rms?: number, peak?: number, dbfs?: number, path?: string,
            error?: string}",
    ),
    ("audio_mute", "{muted: boolean}"),
    (
        "camera_sources",
        "{permission: {status: string, fix: string|null}, sources: [{source: string, name: string, kind: string, virtual: boolean,
            manufacturer?: string|null, model?: string|null, transport?: string|null, formats?: [object], description?: string,
            supported?: boolean}], cameras_error?: string}",
    ),
    ("camera_link", r#"{device: string, applied: "live"|"at_boot", links: [@camera_link], name?: string, note?: string}"#),
    (
        "camera_unlink",
        r#"{device: string, unlinked: [string], now: [{facing: string, source: string}], applied: "live"|"at_boot",
            links: [@camera_link]}"#,
    ),
    ("camera_links", "[@camera_link]"),
    ("network_env", "{*: string}"),
    ("console", "[string]"),
    ("rotate", "null|{orientation: string}"),
    (
        "fold",
        "{angle?: number|null, posture?: string, folded?: boolean, display?: string, width?: number, height?: number, postures?: [string]}",
    ),
    ("xr", "[string]"),
    ("touchpad", "null"),
    ("stylus", "null"),
    ("rotary", "null|{delta: number}"),
    ("vehicle", "null|[@reading]|@reading"),
    ("watch_gesture", "null"),
    ("remote_swipe", "null"),
    ("pointer", "null"),
    ("carplay", "null|{display: string, width: number, height: number}"),
];

/// The JSON Schema of a successful `method`'s result, or `None` for a name that isn't in
/// [`METHODS`] (or has no line in [`OUTPUTS`] yet: a test lists those).
pub fn output_schema(method: &str) -> Option<Value> {
    let (_, spec) = OUTPUTS.iter().find(|(m, _)| *m == method)?;
    METHODS.iter().any(|(m, ..)| *m == method).then(|| parse(spec).unwrap_or_else(|e| panic!("OUTPUTS[{method}]: {e}")))
}

/// Parses a type written in the grammar above into a schema.
fn parse(spec: &str) -> Result<Value, String> {
    let mut p = Parser { s: spec.as_bytes(), at: 0 };
    let schema = p.union()?;
    p.space();
    if p.at != p.s.len() {
        return Err(format!("unexpected `{}`", &spec[p.at..]));
    }
    Ok(schema)
}

fn def(name: &str) -> Result<&'static str, String> {
    DEFS.iter().find(|(n, _)| *n == name).map(|(_, d)| *d).ok_or_else(|| format!("no shared type `@{name}`"))
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.s.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.space();
        if self.s.get(self.at) == Some(&c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) { Ok(()) } else { Err(format!("expected `{}` at {}", c as char, self.at)) }
    }

    fn word(&mut self) -> String {
        self.space();
        let start = self.at;
        while self.s.get(self.at).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            self.at += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.at]).into_owned()
    }

    fn quoted(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let start = self.at;
        while self.s.get(self.at).is_some_and(|c| *c != b'"') {
            self.at += 1;
        }
        let text = String::from_utf8_lossy(&self.s[start..self.at]).into_owned();
        self.expect(b'"')?;
        Ok(text)
    }

    fn union(&mut self) -> Result<Value, String> {
        let mut alts = vec![self.single()?];
        while self.eat(b'|') {
            alts.push(self.single()?);
        }
        Ok(join(alts))
    }

    fn single(&mut self) -> Result<Value, String> {
        self.space();
        match self.s.get(self.at) {
            Some(b'"') => Ok(json!({"type": "string", "enum": [self.quoted()?]})),
            Some(b'[') => {
                self.at += 1;
                let items = self.union()?;
                self.expect(b']')?;
                Ok(json!({"type": "array", "items": items}))
            }
            Some(b'{') => {
                self.at += 1;
                self.object()
            }
            Some(b'@') => {
                self.at += 1;
                parse(def(&self.word())?)
            }
            _ => match self.word().as_str() {
                "any" => Ok(json!({})),
                ty @ ("null" | "string" | "number" | "boolean" | "object") => Ok(json!({"type": ty})),
                other => Err(format!("unknown type `{other}` at {}", self.at)),
            },
        }
    }

    /// After `{`: the fields, the spliced shared ones, or `*: T`.
    fn object(&mut self) -> Result<Value, String> {
        if self.eat(b'*') {
            self.expect(b':')?;
            let values = self.union()?;
            self.expect(b'}')?;
            return Ok(json!({"type": "object", "additionalProperties": values}));
        }
        let (mut properties, mut required) = (Map::new(), Vec::new());
        self.fields(&mut properties, &mut required)?;
        self.expect(b'}')?;
        let mut schema = json!({"type": "object", "properties": properties});
        if !required.is_empty() {
            schema["required"] = required.into();
        }
        Ok(schema)
    }

    fn fields(&mut self, properties: &mut Map<String, Value>, required: &mut Vec<Value>) -> Result<(), String> {
        loop {
            self.space();
            match self.s.get(self.at) {
                Some(b'}') | None => return Ok(()),
                Some(b'.') => {
                    for _ in 0..3 {
                        self.expect(b'.')?;
                    }
                    self.expect(b'@')?;
                    let mut inner = Parser { s: def(&self.word())?.as_bytes(), at: 0 };
                    inner.fields(properties, required)?;
                }
                _ => {
                    let name = if self.s.get(self.at) == Some(&b'"') { self.quoted()? } else { self.word() };
                    if name.is_empty() {
                        return Err(format!("expected a field name at {}", self.at));
                    }
                    let optional = self.eat(b'?');
                    self.expect(b':')?;
                    if !optional {
                        required.push(name.clone().into());
                    }
                    properties.insert(name, self.union()?);
                }
            }
            if !self.eat(b',') {
                return Ok(());
            }
        }
    }
}

/// `A | B`: string choices merge into one `enum`, plain types into one `type` list, a nullable
/// schema takes `null` into its `type`; anything else is `anyOf`.
fn join(alts: Vec<Value>) -> Value {
    if alts.len() == 1 {
        return alts.into_iter().next().unwrap_or_default();
    }
    let plain = |v: &Value| v.as_object().is_some_and(|o| o.len() == 1 && o["type"].is_string());
    let choices = |v: &Value| v["type"] == "string" && v["enum"].is_array() && v.as_object().is_some_and(|o| o.len() == 2);
    if alts.iter().all(choices) {
        let all: Vec<Value> = alts.iter().flat_map(|a| a["enum"].as_array().cloned().unwrap_or_default()).collect();
        return json!({"type": "string", "enum": all});
    }
    if alts.iter().all(plain) {
        return json!({"type": alts.iter().map(|a| a["type"].clone()).collect::<Vec<_>>()});
    }
    let null = json!({"type": "null"});
    let rest: Vec<&Value> = alts.iter().filter(|a| **a != null).collect();
    if rest.len() == 1 && rest[0]["type"].is_string() {
        let mut schema = rest[0].clone();
        schema["type"] = json!([rest[0]["type"], "null"]);
        // A string choice that may be null: the null is a value too.
        if let Some(choices) = schema.get_mut("enum").and_then(Value::as_array_mut) {
            choices.push(Value::Null);
        }
        return schema;
    }
    json!({"anyOf": alts})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_says_what_it_returns() {
        let missing: Vec<&str> = METHODS.iter().map(|(m, ..)| *m).filter(|m| output_schema(m).is_none()).collect();
        assert!(missing.is_empty(), "add these to OUTPUTS in outputs.rs: {missing:?}");
        for (m, _) in OUTPUTS {
            assert!(METHODS.iter().any(|(name, ..)| name == m), "{m} is not a method");
        }
        assert_eq!(output_schema("nope"), None);
    }

    #[test]
    fn admission_and_pacing_are_described_once_and_boots_schema_stays_small() {
        // Schemas are inlined (no $ref), and `boot` is in every client's tool list (#292): its
        // output is one object, with the admission and pacing written out once and kept shallow.
        let boot = output_schema("boot").expect("boot").to_string();
        assert!(boot.len() < 2000, "boot's output schema is {} bytes", boot.len());
        for shared in ["\"verdict\":{", "\"admissible\":{", "\"steps\":{"] {
            assert_eq!(boot.matches(shared).count(), 1, "{shared} twice in boot's schema: {boot}");
        }
        for m in ["boot_many", "create", "clone"] {
            let s = output_schema(m).expect(m).to_string();
            assert_eq!(s.matches("\"admissible\":{").count(), 1, "{m}");
        }
        assert!(output_schema("admission_check").is_some_and(|s| s.to_string().contains("\"costs\"")));
    }

    #[test]
    fn the_grammar_reads_types_objects_unions_and_shared_parts() {
        assert_eq!(parse("null"), Ok(json!({"type": "null"})));
        assert_eq!(parse("[string]"), Ok(json!({"type": "array", "items": {"type": "string"}})));
        assert_eq!(
            parse(r#"{a: number, b?: string|null, "1s": "x"|"y"}"#),
            Ok(json!({"type": "object", "properties": {
                "a": {"type": "number"}, "b": {"type": ["string", "null"]}, "1s": {"type": "string", "enum": ["x", "y"]}},
                "required": ["a", "1s"]}))
        );
        assert_eq!(parse("{*: string}"), Ok(json!({"type": "object", "additionalProperties": {"type": "string"}})));
        let rotate = output_schema("rotate").expect("rotate");
        assert_eq!(rotate["type"], json!(["object", "null"]));
        assert_eq!(rotate["required"], json!(["orientation"]));
        let vehicle = output_schema("vehicle").expect("vehicle");
        assert_eq!(vehicle["anyOf"].as_array().map(Vec::len), Some(3));
        let leaks = output_schema("mem_leaks").expect("mem_leaks");
        assert!(leaks["properties"]["pid"].is_object() && leaks["properties"]["leak_count"].is_object());
        assert!(leaks["required"].as_array().is_some_and(|r| r.contains(&json!("pid"))));
        assert_eq!(output_schema("devices").expect("devices")["items"]["properties"]["platform"]["enum"], json!(["ios", "android"]));
        assert_eq!(parse("{a: }").map(|_| ()).map_err(|e| !e.is_empty()), Err(true));
        assert!(parse("wat").is_err() && parse("@nope").is_err() && parse("[string").is_err());
    }
}
