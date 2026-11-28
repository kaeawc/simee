//! Resources: what a client can read without calling a tool. Devices, runtimes and form factors
//! as JSON; for each device its info, its screen (agent-sized) and screenshot (full size), its
//! apps, and on iOS its logs and processes; an app's preferences by template; and the device
//! view's page. Each is a read of the same API method a tool calls, so a client can subscribe to
//! it ([`crate::watch`]) instead of calling the tool again.

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use serde_json::{Value, json};

use crate::app;
use crate::backend::Backend;
use crate::rpc::{INTERNAL_ERROR, RpcError};
use crate::tools::{self, Toolsets};

const SCHEME: &str = "simee://";
pub const DEVICES: &str = "simee://devices";
pub const RUNTIMES: &str = "simee://runtimes";
pub const DEVICE_TYPES: &str = "simee://device-types";
pub const FORM_FACTORS: &str = "simee://form-factors";
pub const CAMERA_SOURCES: &str = "simee://camera/sources";

/// How many of a device's newest log lines the `logs` resource holds.
pub const LOG_TAIL_LINES: usize = 200;
/// How far back the `logs` resource reaches. It reads through the device's log stream (started by
/// the first read), which answers a window only once it has run that long and while its ring still
/// holds it: a just-booted device fills the ring in about half a minute. A subscription reads once
/// a second, so a longer window makes every one of its first reads (and, on a busy device, all of
/// them) a `log show` on the device. Keep it at 5 s or less.
const LOG_WINDOW: &str = "5s";

/// What a resource URI names.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Devices,
    Runtimes,
    DeviceTypes,
    FormFactors,
    /// `simee://camera/sources`: what a camera can link to (`camera_sources`).
    CameraSources,
    /// `simee://devices/{device}`: the device's info.
    Device(String),
    /// `simee://devices/{device}/screen`: the screen, shrunk to what an agent needs.
    Screen(String),
    /// `simee://devices/{device}/screenshot`: the screen at the device's own size.
    Screenshot(String),
    /// `simee://devices/{device}/logs`: the newest [`LOG_TAIL_LINES`] lines of the last [`LOG_WINDOW`].
    Logs(String),
    /// `simee://devices/{device}/apps`
    Apps(String),
    /// `simee://devices/{device}/processes`
    Processes(String),
    /// `simee://devices/{device}/prefs/{bundle_id}` (every store) or `.../{store}` (one).
    Prefs {
        device: String,
        bundle_id: String,
        store: Option<String>,
    },
    /// The device view's page.
    View,
}

impl Target {
    /// The API method this resource reads through. A server serves the resource only if it serves
    /// that tool, so a resource is never a way around a toolset.
    fn method(&self) -> &'static str {
        match self {
            Target::Devices => "devices",
            Target::Runtimes => "runtimes",
            Target::DeviceTypes => "device_types",
            Target::FormFactors => "form_factors",
            Target::CameraSources => "camera_sources",
            Target::Device(_) => "info",
            Target::Screen(_) | Target::Screenshot(_) => "screenshot",
            Target::Logs(_) => "logs",
            Target::Apps(_) => "apps",
            Target::Processes(_) => "processes",
            Target::Prefs { .. } => "prefs_read",
            Target::View => tools::VIEW_TOOL,
        }
    }

    /// The one spelling of the URI: `simee://devices/…`, percent-encoded as [`device_uri`] does.
    fn uri(&self) -> String {
        let leaf = |device: &str, leaf: &str| device_uri(device, leaf);
        match self {
            Target::Devices => DEVICES.into(),
            Target::Runtimes => RUNTIMES.into(),
            Target::DeviceTypes => DEVICE_TYPES.into(),
            Target::FormFactors => FORM_FACTORS.into(),
            Target::CameraSources => CAMERA_SOURCES.into(),
            Target::View => app::URI.into(),
            Target::Device(d) => leaf(d, ""),
            Target::Screen(d) => leaf(d, "screen"),
            Target::Screenshot(d) => leaf(d, "screenshot"),
            Target::Logs(d) => leaf(d, "logs"),
            Target::Apps(d) => leaf(d, "apps"),
            Target::Processes(d) => leaf(d, "processes"),
            Target::Prefs { device, bundle_id, store } => {
                let base = leaf(device, &format!("prefs/{}", encode(bundle_id)));
                store.as_ref().map_or(base.clone(), |s| format!("{base}/{}", encode(s)))
            }
        }
    }
}

/// Refuses a resource whose tool `toolsets` doesn't serve, naming the toolset that has it.
fn gate(target: &Target, toolsets: &Toolsets, uri: &str) -> Result<(), RpcError> {
    let method = target.method();
    if toolsets.contains(method) {
        return Ok(());
    }
    let (set, label) = (tools::toolset_of(method), toolsets.label());
    Err(RpcError::invalid_params(format!(
        "Resource `{uri}` isn't served here: it reads through the `{method}` tool, which is in the `{set}` toolset, and this server serves \
         `{label}`. Start it with {}={label},{set} (or all)",
        tools::TOOLSETS_ENV
    ))
    .with_data(json!({"uri": uri, "tool": method, "toolset": set, "enabled": label})))
}

/// The URI `uri` names in the one spelling every alias shares (`None` for one that isn't ours).
/// Subscriptions are keyed on it, so two spellings of a resource are one read.
pub fn spelling(uri: &str) -> Option<String> {
    parse(uri).map(|target| target.uri())
}

/// [`spelling`], for a resource `toolsets` serves; `None` for any other.
pub fn canonical(uri: &str, toolsets: &Toolsets) -> Option<String> {
    let target = parse(uri)?;
    toolsets.contains(target.method()).then(|| target.uri())
}

/// Percent-encodes everything but unreserved characters, so a device name is one path segment.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn device_uri(device: &str, leaf: &str) -> String {
    let base = format!("{DEVICES}/{}", encode(device));
    if leaf.is_empty() { base } else { format!("{base}/{leaf}") }
}

/// The target a URI names, or `None` for one that isn't this server's. `simee://device/{id}` is
/// read as `simee://devices/{id}`, so either spelling of a device's URI works (a subscription
/// is announced under the spelling the client gave).
pub fn parse(uri: &str) -> Option<Target> {
    if uri == app::URI {
        return Some(Target::View);
    }
    let path = uri.strip_prefix(SCHEME)?;
    match path {
        "devices" => return Some(Target::Devices),
        "runtimes" => return Some(Target::Runtimes),
        "device-types" => return Some(Target::DeviceTypes),
        "form-factors" => return Some(Target::FormFactors),
        "camera/sources" => return Some(Target::CameraSources),
        _ => {}
    }
    let rest = path.strip_prefix("devices/").or_else(|| path.strip_prefix("device/"))?;
    let mut parts = rest.split('/');
    let segment = |part: Option<&str>| part.filter(|p| !p.is_empty()).and_then(decode);
    let device = segment(parts.next())?;
    let Some(leaf) = parts.next() else { return Some(Target::Device(device)) };
    let target = match leaf {
        "screen" => Target::Screen(device),
        "screenshot" => Target::Screenshot(device),
        "logs" => Target::Logs(device),
        "apps" => Target::Apps(device),
        "processes" => Target::Processes(device),
        "prefs" => {
            let bundle_id = segment(parts.next())?;
            let store = match parts.next() {
                None => None,
                Some(store) => Some(segment(Some(store))?),
            };
            Target::Prefs { device, bundle_id, store }
        }
        _ => return None,
    };
    parts.next().is_none().then_some(target)
}

/// `resources/templates/list`: the per-device resources as URI templates, those `toolsets` serves.
pub fn templates(toolsets: &Toolsets) -> Value {
    let all = json!([
        {"uriTemplate": "simee://devices/{device}", "name": "device", "title": "Device info", "mimeType": "application/json",
         "description": "A device's paths, identity and profile (the `info` method). `device` is a UDID or an AVD name."},
        {"uriTemplate": "simee://devices/{device}/screen", "name": "screen", "title": "Screen", "mimeType": "image/png",
         "description": "The device's screen now, shrunk so its longest side is at most 1024 px: what an agent reads. Subscribe to \
                         be told when it changes, at most once a second unless SIMEE_MCP_SCREEN_FPS says more (a booted device)."},
        {"uriTemplate": "simee://devices/{device}/screenshot", "name": "screenshot", "title": "Screenshot", "mimeType": "image/png",
         "description": "The device's screen now at the device's own size, as a PNG (a booted device)."},
        {"uriTemplate": "simee://devices/{device}/logs", "name": "logs", "title": "Device logs", "mimeType": "text/plain",
         "description": "The newest 200 lines of the last 5 seconds of the device's unified log (the `logs` method; iOS). A \
                         subscription says when new lines arrived, once a second at most."},
        {"uriTemplate": "simee://devices/{device}/apps", "name": "apps", "title": "Installed apps", "mimeType": "application/json",
         "description": "The device's installed bundle ids (the `apps` method; iOS)."},
        {"uriTemplate": "simee://devices/{device}/processes", "name": "processes", "title": "Device processes", "mimeType": "application/json",
         "description": "The device's processes with CPU and memory (the `processes` method; iOS). A subscription says when a \
                         process starts or exits, not when its CPU moves."},
        {"uriTemplate": "simee://devices/{device}/prefs/{bundle_id}", "name": "prefs", "title": "App preferences", "mimeType": "application/json",
         "description": "Every key of an app's preference stores, typed (the `prefs_read` method, read fresh). `bundle_id` is the app's \
                         bundle id or package name."},
        {"uriTemplate": "simee://devices/{device}/prefs/{bundle_id}/{store}", "name": "prefs-store", "title": "App preference store",
         "mimeType": "application/json",
         "description": "One preference store of an app (a `store` id from `prefs_stores`). A subscription says when a key changes."},
    ]);
    let served = |t: &Value| {
        let sample = t["uriTemplate"].as_str().unwrap_or("").replace("{device}", "d").replace("{bundle_id}", "b").replace("{store}", "s");
        parse(&sample).is_some_and(|target| toolsets.contains(target.method()))
    };
    Value::Array(all.as_array().into_iter().flatten().filter(|t| served(t)).cloned().collect())
}

/// `resources/list`: the fixed resources, then each device's. A screen is listed for a booted
/// device; logs and processes for a booted iOS one, apps for an iOS one (Android's console is
/// the `console` tool). Preferences are by template only: every app has some. Only what `toolsets`
/// serves is listed.
pub fn list(backend: &dyn Backend, toolsets: &Toolsets) -> Vec<Value> {
    let mut out = vec![
        json!({"uri": DEVICES, "name": "devices", "title": "Devices", "mimeType": "application/json",
            "description": "Every device on both platforms: UDID, name, state, platform, form factor."}),
        json!({"uri": RUNTIMES, "name": "runtimes", "title": "Runtimes", "mimeType": "application/json",
            "description": "Installed runtimes: iOS runtimes and Android system images."}),
        json!({"uri": DEVICE_TYPES, "name": "device-types", "title": "Device types", "mimeType": "application/json",
            "description": "iOS device types a simulator can be created from."}),
        json!({"uri": FORM_FACTORS, "name": "form-factors", "title": "Form factors", "mimeType": "application/json",
            "description": "Every form factor on every platform, with its inputs, or why it is unsupported."}),
        json!({"uri": CAMERA_SOURCES, "name": "camera-sources", "title": "Camera sources", "mimeType": "application/json",
            "description": "Every source a device camera can link to (the host's cameras, synthetic sources) and the host's camera permission."}),
        app::listing(),
    ];
    // Without the `devices` tool the server doesn't say which devices there are, and so lists none.
    let devices = if toolsets.contains("devices") { backend.call("devices", &json!({})).unwrap_or(Value::Null) } else { Value::Null };
    for d in devices.as_array().into_iter().flatten() {
        let (Some(udid), name) = (d["udid"].as_str(), d["name"].as_str().unwrap_or("device")) else { continue };
        let platform = d["platform"].as_str().unwrap_or("");
        let booted = d["state"] == "Booted";
        let label = |what: &str| format!("{name} {what} ({platform})");
        out.push(json!({"uri": device_uri(udid, ""), "name": label("info"), "mimeType": "application/json",
            "description": format!("{name}: state {}", d["state"].as_str().unwrap_or("unknown"))}));
        if booted {
            out.push(json!({"uri": device_uri(udid, "screen"), "name": label("screen"), "mimeType": "image/png",
                "description": format!("{name}'s screen, agent-sized")}));
            out.push(json!({"uri": device_uri(udid, "screenshot"), "name": label("screenshot"), "mimeType": "image/png"}));
        }
        if platform == "ios" {
            if booted {
                out.push(json!({"uri": device_uri(udid, "logs"), "name": label("logs"), "mimeType": "text/plain"}));
                out.push(json!({"uri": device_uri(udid, "processes"), "name": label("processes"), "mimeType": "application/json"}));
            }
            out.push(json!({"uri": device_uri(udid, "apps"), "name": label("apps"), "mimeType": "application/json"}));
        }
    }
    out.retain(|r| r["uri"].as_str().is_some_and(|uri| canonical(uri, toolsets).is_some()));
    out
}

/// A method's failure as a resource error: a device or a resource its platform lacks is not found;
/// anything else (a device that isn't running) is the server's to report.
fn failed(uri: &str, message: String) -> RpcError {
    if message.contains("no device matches") || message.contains("isn't available on") || message.contains("not yet available on") {
        RpcError::invalid_params("Resource not found").with_data(json!({"uri": uri, "reason": message}))
    } else {
        RpcError::new(INTERNAL_ERROR, message).with_data(json!({"uri": uri}))
    }
}

fn json_text(uri: &str, value: &Value) -> Value {
    json!({"uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(value).unwrap_or_default()})
}

/// A device's screenshot at its own size, base64.
fn screenshot_png(backend: &dyn Backend, uri: &str, device: &str) -> Result<String, RpcError> {
    let shot = backend.call("screenshot", &json!({"device": device})).map_err(|e| failed(uri, e))?;
    shot["png"].as_str().map(str::to_owned).ok_or_else(|| RpcError::new(INTERNAL_ERROR, "the screenshot has no image"))
}

/// A device's log lines as one text, the newest [`LOG_TAIL_LINES`].
fn log_tail(backend: &dyn Backend, uri: &str, device: &str) -> Result<String, RpcError> {
    let params = json!({"device": device, "last": LOG_WINDOW, "stream": true});
    let logs = backend.call("logs", &params).map_err(|e| failed(uri, e))?;
    let text = match &logs {
        Value::String(s) => s.clone(),
        Value::Array(lines) if lines.iter().all(Value::is_string) => lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"),
        other => return Ok(serde_json::to_string_pretty(other).unwrap_or_default()),
    };
    let lines: Vec<&str> = text.lines().collect();
    Ok(lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n"))
}

/// The method call a prefs resource reads.
fn prefs_call(backend: &dyn Backend, uri: &str, device: &str, bundle_id: &str, store: Option<&str>) -> Result<Value, RpcError> {
    let mut params = json!({"device": device, "bundle_id": bundle_id, "fresh": true});
    if let Some(store) = store {
        params["store"] = store.into();
    }
    backend.call("prefs_read", &params).map_err(|e| failed(uri, e))
}

fn not_found(uri: &str) -> RpcError {
    RpcError::invalid_params("Resource not found").with_data(json!({"uri": uri}))
}

/// Whether `uri` names a resource this server serves: not found if it isn't one of ours, refused
/// if `toolsets` doesn't serve what it reads through.
pub fn check(uri: &str, toolsets: &Toolsets) -> Result<(), RpcError> {
    gate(&parse(uri).ok_or_else(|| not_found(uri))?, toolsets, uri)
}

/// `resources/read`: the contents of `uri`, if `toolsets` serves what it reads through.
pub fn read(backend: &dyn Backend, toolsets: &Toolsets, uri: &str) -> Result<Vec<Value>, RpcError> {
    let target = parse(uri).ok_or_else(|| not_found(uri))?;
    gate(&target, toolsets, uri)?;
    fetch(backend, target, uri)
}

/// The contents of a resource, whatever the server serves.
fn fetch(backend: &dyn Backend, target: Target, uri: &str) -> Result<Vec<Value>, RpcError> {
    let call = |method: &str, params: Value| backend.call(method, &params).map_err(|e| failed(uri, e));
    let content = match target {
        Target::View => app::contents(),
        Target::Devices => json_text(uri, &call("devices", json!({}))?),
        Target::Runtimes => json_text(uri, &call("runtimes", json!({}))?),
        Target::DeviceTypes => json_text(uri, &call("device_types", json!({}))?),
        Target::FormFactors => json_text(uri, &call("form_factors", json!({}))?),
        Target::CameraSources => json_text(uri, &call("camera_sources", json!({}))?),
        Target::Device(d) => json_text(uri, &call("info", json!({"device": d}))?),
        Target::Apps(d) => json_text(uri, &call("apps", json!({"device": d}))?),
        Target::Processes(d) => json_text(uri, &call("processes", json!({"device": d}))?),
        Target::Prefs { device, bundle_id, store } => json_text(uri, &prefs_call(backend, uri, &device, &bundle_id, store.as_deref())?),
        Target::Screenshot(d) => json!({"uri": uri, "mimeType": "image/png", "blob": screenshot_png(backend, uri, &d)?}),
        Target::Screen(d) => {
            let png = screenshot_png(backend, uri, &d)?;
            let small = sim_api::shrink_png_base64(&png, sim_api::AGENT_MAX_SIDE)
                .map_err(|e| RpcError::new(INTERNAL_ERROR, e).with_data(json!({"uri": uri})))?;
            json!({"uri": uri, "mimeType": "image/png", "blob": small})
        }
        Target::Logs(d) => json!({"uri": uri, "mimeType": "text/plain", "text": log_tail(backend, uri, &d)?}),
    };
    Ok(vec![content])
}

fn hash_of(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// A resource's contents as a number that changes when what a subscriber cares about does
/// (`None` when it can't be read). Most resources change with their bytes. The others skip what
/// moves without meaning: a screen is compared as the device made it, so an unchanged frame is
/// never shrunk; logs by their newest line, so lines aging out of the window say nothing; the
/// processes by who is running, not by how busy they are.
pub fn fingerprint(backend: &dyn Backend, uri: &str) -> Option<u64> {
    match parse(uri)? {
        Target::Screen(d) | Target::Screenshot(d) => screenshot_png(backend, uri, &d).ok().map(|png| hash_of(&png)),
        Target::Logs(d) => log_tail(backend, uri, &d).ok().map(|text| hash_of(text.lines().rev().find(|l| !l.is_empty()).unwrap_or(""))),
        Target::Processes(d) => {
            let list = backend.call("processes", &json!({"device": d})).ok()?;
            let Some(rows) = list.as_array() else { return Some(hash_of(&list.to_string())) };
            let mut who: Vec<String> = rows
                .iter()
                .map(|p| format!("{} {}", p["pid"], p["path"].as_str().unwrap_or_else(|| p["name"].as_str().unwrap_or(""))))
                .collect();
            who.sort();
            Some(hash_of(&who.join("\n")))
        }
        other => fetch(backend, other, uri).ok().map(|contents| hash_of(&Value::Array(contents).to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::Fake;

    #[test]
    fn the_logs_window_is_short_enough_for_the_stream_to_answer_every_read_of_a_subscription() {
        // The stream answers a window only after running that long, and a just-booted device's
        // ring holds half a minute: a subscription reading once a second falls back to `log show`
        // for every read of a window near that.
        let seconds: u64 = LOG_WINDOW.strip_suffix('s').and_then(|n| n.parse().ok()).expect("a window in seconds");
        assert!((1..=5).contains(&seconds), "{LOG_WINDOW}");
    }

    /// A real PNG of `width` x `height`, base64, as `screenshot` answers it.
    fn png(width: u32, height: u32) -> String {
        use base64::Engine as _;
        let frame = sim_core::frame::Frame { width, height, rgba: vec![0x40; width as usize * height as usize * 4] };
        base64::engine::general_purpose::STANDARD.encode(sim_core::frame::encode_png(&frame).expect("png"))
    }

    const IPHONE: &str = "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000";

    #[test]
    fn uris_round_trip_and_junk_is_refused() {
        for (device, leaf, target) in [
            ("phone", "", Target::Device("phone".into())),
            (IPHONE, "screenshot", Target::Screenshot(IPHONE.into())),
            (IPHONE, "screen", Target::Screen(IPHONE.into())),
            ("My Phone/2", "logs", Target::Logs("My Phone/2".into())),
            ("é", "apps", Target::Apps("é".into())),
            ("phone", "processes", Target::Processes("phone".into())),
        ] {
            let uri = device_uri(device, leaf);
            assert!(!uri[SCHEME.len()..].contains(' ') && uri.matches('/').count() == 3 + usize::from(!leaf.is_empty()), "{uri}");
            assert_eq!(parse(&uri), Some(target), "{uri}");
        }
        let prefs = |store: Option<&str>| Target::Prefs {
            device: "p".into(),
            bundle_id: "com.example.app".into(),
            store: store.map(str::to_owned),
        };
        assert_eq!(parse("simee://devices/p/prefs/com.example.app"), Some(prefs(None)));
        assert_eq!(parse("simee://devices/p/prefs/com.example.app/group%2Fa%20b"), Some(prefs(Some("group/a b"))));
        // The singular spelling is the same resource.
        assert_eq!(parse("simee://device/p"), Some(Target::Device("p".into())));
        assert_eq!(parse("simee://device/p/screen"), Some(Target::Screen("p".into())));
        assert_eq!(parse("simee://device/p/prefs/com.example.app"), Some(prefs(None)));
        assert_eq!(parse(DEVICES), Some(Target::Devices));
        assert_eq!(parse(RUNTIMES), Some(Target::Runtimes));
        assert_eq!(parse(DEVICE_TYPES), Some(Target::DeviceTypes));
        assert_eq!(parse(FORM_FACTORS), Some(Target::FormFactors));
        assert_eq!(parse(CAMERA_SOURCES), Some(Target::CameraSources));
        assert_eq!(parse(app::URI), Some(Target::View));
        for junk in [
            "",
            "simee://",
            "simee://devices/",
            "simee://device/",
            "simee://devices/x/nope",
            "simee://devices/x/logs/more",
            "simee://devices/x/processes/1",
            "simee://devices/x/prefs",
            "simee://devices/x/prefs/",
            "simee://devices/x/prefs/b/",
            "simee://devices/x/prefs/b/s/more",
            "simee://devices/x/prefs/%zz",
            "simee://devices/%zz",
            "file:///x",
            "simee://other",
        ] {
            assert_eq!(parse(junk), None, "{junk}");
        }
    }

    #[test]
    fn the_list_has_the_fixed_resources_and_each_devices() {
        let fake = Fake::with_devices();
        let list = list(&fake, &Toolsets::all());
        let uris: Vec<&str> = list.iter().filter_map(|r| r["uri"].as_str()).collect();
        for fixed in [DEVICES, RUNTIMES, DEVICE_TYPES, FORM_FACTORS, CAMERA_SOURCES, app::URI] {
            assert!(uris.contains(&fixed), "{fixed}");
        }
        // The booted Android device: info and both screens, no iOS-only logs, apps or processes.
        for leaf in ["", "/screen", "/screenshot"] {
            assert!(uris.contains(&format!("simee://devices/phone{leaf}").as_str()), "{leaf}");
        }
        for leaf in ["/logs", "/apps", "/processes"] {
            assert!(!uris.contains(&format!("simee://devices/phone{leaf}").as_str()), "{leaf}");
        }
        // The shut-down iPhone: info and apps, but nothing that needs it running.
        assert!(
            uris.contains(&format!("simee://devices/{IPHONE}").as_str())
                && uris.contains(&format!("simee://devices/{IPHONE}/apps").as_str())
        );
        for leaf in ["screen", "screenshot", "logs", "processes"] {
            assert!(!uris.contains(&format!("simee://devices/{IPHONE}/{leaf}").as_str()), "{leaf}");
        }
        assert!(list.iter().all(|r| r["name"].is_string() && r["uri"].is_string()));
        // A booted iPhone has them all.
        fake.answer("devices", Ok(json!([{"udid": IPHONE, "name": "iPhone 17", "state": "Booted", "platform": "ios"}])));
        let booted = super::list(&fake, &Toolsets::all());
        let uris: Vec<&str> = booted.iter().filter_map(|r| r["uri"].as_str()).collect();
        for leaf in ["", "/screen", "/screenshot", "/logs", "/apps", "/processes"] {
            assert!(uris.contains(&format!("simee://devices/{IPHONE}{leaf}").as_str()), "{leaf}");
        }
        // No devices (or a failing listing) is just the fixed ones.
        let none = Fake::default();
        none.answer("devices", Err("boom".into()));
        assert_eq!(super::list(&none, &Toolsets::all()).len(), 6);
    }

    #[test]
    fn templates_cover_the_per_device_resources() {
        let templates = templates(&Toolsets::all());
        let names: Vec<&str> = templates.as_array().expect("array").iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["device", "screen", "screenshot", "logs", "apps", "processes", "prefs", "prefs-store"]);
        for t in templates.as_array().expect("array") {
            let uri =
                t["uriTemplate"].as_str().expect("template").replace("{device}", "d").replace("{bundle_id}", "b").replace("{store}", "s");
            assert!(parse(&uri).is_some(), "{uri}");
        }
    }

    #[test]
    fn reading_calls_the_method_and_shapes_the_contents() {
        let fake = Fake::with_devices();
        fake.answer("runtimes", Ok(json!([{"name": "Android 16"}])));
        fake.answer("info", Ok(json!({"udid": "phone", "profile": {}})));
        fake.answer("screenshot", Ok(json!({"width": 2, "height": 1, "png": "iVBOR"})));
        fake.answer("logs", Ok(json!(["line one", "line two"])));
        fake.answer("apps", Ok(json!(["com.example.app"])));
        fake.answer("processes", Ok(json!([{"pid": 1, "path": "/sbin/launchd", "cpu": 0.5}])));
        fake.answer("prefs_read", Ok(json!({"store": "com.example.app", "values": {"theme": {"type": "string", "value": "dark"}}})));
        let read1 = |uri: &str| read(&fake, &Toolsets::all(), uri).expect(uri).remove(0);
        let devices = read1(DEVICES);
        assert_eq!(devices["mimeType"], "application/json");
        assert!(devices["text"].as_str().is_some_and(|t| t.contains("iPhone 17")));
        assert!(read1(RUNTIMES)["text"].as_str().is_some_and(|t| t.contains("Android 16")));
        assert_eq!(fake.called("info").len(), 0);
        let info = read1("simee://devices/phone");
        assert!(info["text"].as_str().is_some_and(|t| t.contains("profile")));
        assert_eq!(fake.called("info"), [json!({"device": "phone"})]);
        // The full-size screenshot passes the API's bytes through untouched.
        assert_eq!(
            read1("simee://devices/phone/screenshot"),
            json!({"uri": "simee://devices/phone/screenshot", "mimeType": "image/png", "blob": "iVBOR"})
        );
        assert_eq!(read1("simee://devices/phone/logs")["text"], "line one\nline two");
        // Through the device's log stream, so a poll doesn't spawn a `log show` each time.
        assert_eq!(fake.called("logs"), [json!({"device": "phone", "last": "5s", "stream": true})]);
        assert!(read1("simee://devices/phone/apps")["text"].as_str().is_some_and(|t| t.contains("com.example.app")));
        let procs = read1("simee://devices/phone/processes");
        assert!(procs["text"].as_str().is_some_and(|t| t.contains("/sbin/launchd")) && procs["mimeType"] == "application/json");
        assert_eq!(fake.called("processes"), [json!({"device": "phone"})]);
        let prefs = read1("simee://devices/phone/prefs/com.example.app");
        assert!(prefs["text"].as_str().is_some_and(|t| t.contains("\"theme\"")));
        read1("simee://devices/phone/prefs/com.example.app/group.x");
        assert_eq!(
            fake.called("prefs_read"),
            [
                json!({"device": "phone", "bundle_id": "com.example.app", "fresh": true}),
                json!({"device": "phone", "bundle_id": "com.example.app", "fresh": true, "store": "group.x"})
            ]
        );
        let page = read1(app::URI);
        assert_eq!((page["mimeType"].clone(), page["uri"].clone()), (json!(app::MIME), json!(app::URI)));
        assert_eq!(read1(FORM_FACTORS)["mimeType"], "application/json");
        assert_eq!(read1(DEVICE_TYPES)["mimeType"], "application/json");
        fake.answer("camera_sources", Ok(json!({"permission": {"status": "authorized"}, "sources": [{"source": "qr:hello"}]})));
        let sources = read1(CAMERA_SOURCES);
        assert_eq!((sources["uri"].clone(), sources["mimeType"].clone()), (json!(CAMERA_SOURCES), json!("application/json")));
        assert!(sources["text"].as_str().is_some_and(|t| t.contains("qr:hello")));
        assert_eq!(fake.called("camera_sources"), [json!({})]);
        // Logs that aren't lines are shown as JSON.
        fake.answer("logs", Ok(json!({"lines": 2})));
        assert!(read1("simee://devices/phone/logs")["text"].as_str().is_some_and(|t| t.contains("\"lines\": 2")));
    }

    #[test]
    fn the_screen_is_agent_sized_and_the_screenshot_is_not() {
        let fake = Fake::with_devices();
        let full = png(1206, 2622);
        fake.answer("screenshot", Ok(json!({"width": 1206, "height": 2622, "png": full})));
        let screen = read(&fake, &Toolsets::all(), "simee://device/phone/screen").expect("screen").remove(0);
        assert_eq!((screen["uri"].clone(), screen["mimeType"].clone()), (json!("simee://device/phone/screen"), json!("image/png")));
        let blob = screen["blob"].as_str().expect("blob");
        assert!(blob.len() < full.len() / 4, "{} bytes of {}", blob.len(), full.len());
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(blob).expect("base64");
        assert_eq!(sim_core::frame::png_size(&bytes), Some((402, 874)));
        let shot = read(&fake, &Toolsets::all(), "simee://devices/phone/screenshot").expect("screenshot").remove(0);
        assert_eq!(shot["blob"], json!(full));
        assert_eq!(fake.called("screenshot"), [json!({"device": "phone"}), json!({"device": "phone"})]);
        // Not a PNG: the server's error, not an empty image.
        fake.answer("screenshot", Ok(json!({"png": "iVBOR"})));
        assert_eq!(read(&fake, &Toolsets::all(), "simee://devices/phone/screen").expect_err("garbage").code, INTERNAL_ERROR);
    }

    #[test]
    fn logs_are_the_newest_lines() {
        let fake = Fake::with_devices();
        let lines: Vec<String> = (0..LOG_TAIL_LINES + 50).map(|n| format!("line {n}")).collect();
        fake.answer("logs", Ok(json!(lines)));
        let text =
            read(&fake, &Toolsets::all(), "simee://devices/phone/logs").expect("logs").remove(0)["text"].as_str().expect("text").to_owned();
        let got: Vec<&str> = text.lines().collect();
        assert_eq!((got.len(), got[0], got[got.len() - 1]), (LOG_TAIL_LINES, "line 50", "line 249"));
        // A text answer is cut the same way, and a short one is whole.
        fake.answer("logs", Ok(json!("a\nb\nc")));
        assert_eq!(read(&fake, &Toolsets::all(), "simee://devices/phone/logs").expect("logs").remove(0)["text"], "a\nb\nc");
    }

    #[test]
    fn a_resource_that_is_not_there_is_invalid_params_and_other_failures_internal() {
        let fake = Fake::default();
        fake.answer("info", Err("no device matches `ghost`".into()));
        fake.answer("logs", Err("`logs` isn't available on Android devices (it works on iOS)".into()));
        fake.answer("screenshot", Err("phone isn't running; boot it first".into()));
        fake.answer("prefs_read", Err("app preferences (`prefs_*`) are not yet available on Android devices".into()));
        let err = read(&fake, &Toolsets::all(), "simee://devices/ghost").expect_err("missing");
        assert_eq!((err.code, err.message.as_str()), (crate::rpc::INVALID_PARAMS, "Resource not found"));
        assert_eq!(err.data.as_ref().map(|d| d["uri"].clone()), Some(json!("simee://devices/ghost")));
        assert_eq!(read(&fake, &Toolsets::all(), "simee://devices/phone/logs").expect_err("android").code, crate::rpc::INVALID_PARAMS);
        assert_eq!(
            read(&fake, &Toolsets::all(), "simee://devices/phone/prefs/a.b").expect_err("android prefs").code,
            crate::rpc::INVALID_PARAMS
        );
        for leaf in ["screenshot", "screen"] {
            let down = read(&fake, &Toolsets::all(), &format!("simee://devices/phone/{leaf}")).expect_err("stopped");
            assert_eq!((down.code, down.message.as_str()), (INTERNAL_ERROR, "phone isn't running; boot it first"));
        }
        assert_eq!(read(&fake, &Toolsets::all(), "simee://nope").expect_err("not ours").code, crate::rpc::INVALID_PARAMS);
        // A screenshot written to a path has no image to return.
        fake.answer("screenshot", Ok(json!({"path": "/tmp/x.png"})));
        assert_eq!(read(&fake, &Toolsets::all(), "simee://devices/phone/screenshot").expect_err("no png").code, INTERNAL_ERROR);
    }

    #[test]
    fn what_a_subscriber_cares_about_is_what_changes_the_fingerprint() {
        let fake = Fake::with_devices();
        let print = |uri: &str| fingerprint(&fake, uri);
        // Logs: a line aging out of the window is nothing; a new one is something.
        fake.answer("logs", Ok(json!(["a", "b", "c"])));
        let before = print("simee://devices/phone/logs");
        fake.answer("logs", Ok(json!(["b", "c"])));
        assert_eq!(print("simee://devices/phone/logs"), before);
        fake.answer("logs", Ok(json!(["b", "c", "d"])));
        assert_ne!(print("simee://devices/phone/logs"), before);
        // Processes: busier is nothing; one more, or a different one, is something.
        fake.answer("processes", Ok(json!([{"pid": 1, "path": "/a", "cpu": 1.0, "rss_kb": 10}, {"pid": 2, "path": "/b", "cpu": 2.0}])));
        let before = print("simee://devices/phone/processes");
        fake.answer("processes", Ok(json!([{"pid": 2, "path": "/b", "cpu": 9.0}, {"pid": 1, "path": "/a", "cpu": 0.0, "rss_kb": 99}])));
        assert_eq!(print("simee://devices/phone/processes"), before);
        fake.answer("processes", Ok(json!([{"pid": 1, "path": "/a"}, {"pid": 3, "path": "/b"}])));
        assert_ne!(print("simee://devices/phone/processes"), before);
        // A screen is the device's frame: the same bytes are the same, without shrinking them.
        fake.answer("screenshot", Ok(json!({"png": "AAAA"})));
        let before = print("simee://devices/phone/screen");
        assert!(before.is_some() && print("simee://devices/phone/screen") == before);
        assert_eq!(print("simee://devices/phone/screenshot"), before);
        fake.answer("screenshot", Ok(json!({"png": "BBBB"})));
        assert_ne!(print("simee://devices/phone/screen"), before);
        // Anything else changes with its bytes; what can't be read has none.
        fake.answer("apps", Ok(json!(["a"])));
        let before = print("simee://devices/phone/apps");
        fake.answer("apps", Ok(json!(["a", "b"])));
        assert_ne!(print("simee://devices/phone/apps"), before);
        fake.answer("apps", Err("gone".into()));
        assert_eq!(print("simee://devices/phone/apps"), None);
        assert_eq!(print("simee://nope"), None);
    }

    #[test]
    fn a_resource_is_served_only_with_the_toolset_of_the_tool_it_reads_through() {
        let fake = Fake::with_devices();
        fake.answer("processes", Ok(json!([{"pid": 1, "path": "/a"}])));
        fake.answer("prefs_read", Ok(json!({"values": {"token": {"type": "string", "value": "secret"}}})));
        fake.answer("camera_sources", Ok(json!({})));
        let core = Toolsets::default();
        let secrets = [format!("simee://devices/{IPHONE}/processes"), "simee://devices/phone/prefs/com.bank.app".to_owned()];
        for uri in secrets.iter().map(String::as_str).chain(["simee://device/phone/prefs/com.bank.app/s", CAMERA_SOURCES, RUNTIMES]) {
            let err = read(&fake, &core, uri).expect_err(uri);
            assert_eq!(err.code, crate::rpc::INVALID_PARAMS, "{uri}");
            assert!(err.message.contains("isn't served here") && err.message.contains("SIMEE_MCP_TOOLSETS=core,"), "{}", err.message);
            assert_eq!(check(uri, &core).expect_err(uri).message, err.message);
            assert_eq!(canonical(uri, &core), None, "{uri}");
        }
        assert_eq!(
            fake.called("processes").len() + fake.called("prefs_read").len() + fake.called("camera_sources").len(),
            0,
            "never called"
        );
        let err = read(&fake, &core, "simee://devices/phone/prefs/b").expect_err("prefs");
        assert_eq!(err.data.as_ref().map(|d| (d["toolset"].clone(), d["tool"].clone())), Some((json!("prefs"), json!("prefs_read"))));
        // What core serves reads, and the toolset that has the rest unlocks it.
        for uri in
            [DEVICES, "simee://devices/phone", "simee://devices/phone/screen", "simee://devices/phone/apps", "simee://devices/phone/logs"]
        {
            assert_eq!(canonical(uri, &core).as_deref(), Some(uri));
        }
        assert!(read(&fake, &Toolsets::parse("core,debug").expect("sets"), &secrets[0]).is_ok());
        assert!(read(&fake, &Toolsets::parse("core,debug").expect("sets"), &secrets[1]).is_err());
        assert!(read(&fake, &Toolsets::parse("core,prefs").expect("sets"), &secrets[1]).is_ok());
        // Nor are they listed or offered as templates.
        let uris =
            |sets: &Toolsets| -> Vec<String> { list(&fake, sets).iter().filter_map(|r| r["uri"].as_str().map(str::to_owned)).collect() };
        fake.answer("devices", Ok(json!([{"udid": IPHONE, "name": "iPhone 17", "state": "Booted", "platform": "ios"}])));
        assert!(!uris(&core).iter().any(|u| u.ends_with("/processes") || u == RUNTIMES || u == CAMERA_SOURCES || u == FORM_FACTORS));
        assert!(uris(&core).contains(&format!("simee://devices/{IPHONE}/logs")));
        assert!(uris(&Toolsets::parse("core,debug").expect("sets")).contains(&format!("simee://devices/{IPHONE}/processes")));
        let names = |sets: &Toolsets| -> Vec<String> {
            templates(sets).as_array().expect("array").iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect()
        };
        assert_eq!(names(&core), ["device", "screen", "screenshot", "logs", "apps"]);
        assert_eq!(names(&Toolsets::all()).len(), 8);
        // Without `devices` no device is named at all, and the device calls aren't made.
        let none = Fake::with_devices();
        let blind = Toolsets::parse("debug").expect("sets");
        assert!(list(&none, &blind).iter().all(|r| r["uri"].as_str().is_some_and(|u| !u.contains("/devices/"))));
        assert!(none.called("devices").is_empty());
    }

    #[test]
    fn every_spelling_of_a_resource_has_one_canonical_form() {
        let all = Toolsets::all();
        assert_eq!(canonical("simee://device/My%20Phone/screen", &all).as_deref(), Some("simee://devices/My%20Phone/screen"));
        assert_eq!(canonical("simee://devices/My%20Phone/screen", &all).as_deref(), Some("simee://devices/My%20Phone/screen"));
        assert_eq!(canonical("simee://devices/%41/prefs/a%2Eb/g%2Fx", &all).as_deref(), Some("simee://devices/A/prefs/a.b/g%2Fx"));
        for uri in [DEVICES, RUNTIMES, CAMERA_SOURCES, app::URI, "simee://devices/x/processes", "simee://devices/x/prefs/b"] {
            assert_eq!(spelling(uri).as_deref(), Some(uri));
        }
        assert_eq!(spelling("simee://nope"), None);
    }
}
