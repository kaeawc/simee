//! Prompts: simee's common workflows as text an agent follows. Each names the tools to call, in
//! order, with the toolset each is in and whether this server serves it, and the end state to
//! reach. The tools a prompt may name are a list here, which a test checks against the tool table
//! (a renamed tool fails it) and against the rendered text, where a tool is the only thing written
//! in backticks.

use serde_json::{Map, Value, json};
use sim_api::catalog;

use crate::rpc::{Ctx, RpcError};
use crate::tools::{CORE, TOOLSETS_ENV, Toolsets};

/// One argument a prompt takes. MCP prompt arguments are strings.
pub struct Arg {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub required: bool,
    /// The values it may take (empty: any).
    pub choices: &'static [&'static str],
    /// Names separated by commas, at least one.
    pub list: bool,
}

/// A workflow.
pub struct Prompt {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub arguments: &'static [Arg],
    /// Every tool its text may name.
    pub tools: &'static [&'static str],
    render: fn(&Args) -> String,
}

const fn arg(name: &'static str, title: &'static str, description: &'static str, required: bool) -> Arg {
    Arg { name, title, description, required, choices: &[], list: false }
}

const DEVICE: Arg = arg("device", "Device", "A UDID or name (default: the booted device)", false);

/// The prompts, in the order `prompts/list` gives them.
pub const PROMPTS: &[Prompt] = &[
    Prompt {
        name: "boot-and-install",
        title: "Boot a device and install an app",
        description: "Find or create a device of a form factor, boot it, install and launch an app on it",
        arguments: &[
            Arg { choices: &["ios", "android"], ..arg("platform", "Platform", "ios or android", true) },
            arg("form_factor", "Form factor", "phone, tablet, foldable, watch, tv, auto or vision", true),
            arg("app_path", "App", "Path on the host of the .app (iOS) or .apk/.aab (Android)", true),
        ],
        tools: &["devices", "form_factors", "runtimes", "device_types", "create", "boot", "install", "apps", "launch", "screenshot"],
        render: boot_and_install,
    },
    Prompt {
        name: "reproduce-bug",
        title: "Reproduce a bug",
        description: "Follow a bug report's steps on a device, capturing logs, screenshots or both",
        arguments: &[
            DEVICE,
            arg("steps", "Steps", "The bug report's steps to reproduce", true),
            Arg {
                choices: &["logs", "recording", "both"],
                ..arg("capture", "Capture", "logs, recording (a screenshot after each step) or both", true)
            },
        ],
        tools: &["devices", "screen", "screenshot", "logs", "tap", "swipe", "type", "key", "button", "crash_reports"],
        render: reproduce_bug,
    },
    Prompt {
        name: "audit-contrast",
        title: "Audit a screen's contrast",
        description: "Check a screen for text and controls below WCAG contrast, in light and dark",
        arguments: &[DEVICE, arg("screen_description", "Screen", "The screen to audit, if not the one showing", false)],
        tools: &["devices", "screenshot", "tap", "swipe", "button", "contrast_check"],
        render: audit_contrast,
    },
    Prompt {
        name: "find-slowness",
        title: "Find what is slow",
        description: "Find why a device is slow: the host, the device's load beside the others, or a process",
        arguments: &[arg("device", "Device", "A UDID or name", true)],
        tools: &["host", "devices_usage", "processes", "mem_vmmap", "mem_heap", "mem_leaks"],
        render: find_slowness,
    },
    Prompt {
        name: "across-form-factors",
        title: "Test an app across form factors",
        description: "Boot one device per form factor as the host admits them, install the app and screenshot each",
        arguments: &[
            arg("app_path", "App", "Path on the host of the .app (iOS) or .apk/.aab (Android)", true),
            Arg { list: true, ..arg("form_factors", "Form factors", "Form factors separated by commas (phone,tablet,foldable)", true) },
        ],
        tools: &["form_factors", "devices", "create", "boot_many", "install", "apps", "launch", "screenshot", "shutdown"],
        render: across_form_factors,
    },
];

/// A prompt's arguments, checked.
pub struct Args(Map<String, Value>);

impl Args {
    /// The argument's value, if it was given and isn't blank.
    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    }

    /// A required argument (checked before rendering).
    fn req(&self, name: &str) -> &str {
        self.get(name).unwrap_or_default()
    }
}

/// The device a prompt works on, as its text names it.
fn device(a: &Args) -> String {
    match a.get("device") {
        Some(d) => format!("\"{d}\""),
        None => "the booted device (`devices` {booted: true}; ask which if several are)".into(),
    }
}

fn boot_and_install(a: &Args) -> String {
    let (p, f, app) = (a.req("platform"), a.req("form_factor"), a.req("app_path"));
    format!(
        "Boot a {p} {f} device and install {app} on it.\n\
         1. `devices` {{platform: \"{p}\", form_factor: \"{f}\"}}: use a booted one, else one that is shut down.\n\
         2. None: `form_factors` {{platform: \"{p}\", form_factor: \"{f}\"}} says whether {p} has it (stop and say why if not); \
         `runtimes` {{platform: \"{p}\"}} (iOS: and `device_types`), then `create` {{name, platform: \"{p}\", form_factor: \"{f}\"}} \
         with the newest runtime (iOS) or system image (Android).\n\
         3. `boot` {{device, admission: \"wait\"}}: waits until the host can take it (a task to poll, for a client with tasks).\n\
         4. `install` {{device, path: \"{app}\"}}; `apps` {{device}} for its bundle id (Android: package); `launch` {{device, bundle_id}}.\n\
         5. `screenshot` {{device}}.\n\
         Done when: a booted {p} {f} device runs the app in front and the screenshot shows it. Report the device's name and id, \
         the bundle id, and any step that failed with its error."
    )
}

fn reproduce_bug(a: &Args) -> String {
    let d = device(a);
    let capture = a.req("capture");
    let logs = capture != "recording";
    let shots = capture != "logs";
    let mut text = format!("Reproduce this bug on {d}.\nSteps from the report:\n{}\n\n", a.req("steps"));
    text.push_str("1. `screen` {device} for its size in pixels, then `screenshot` {device} for where it starts.\n");
    if logs {
        text.push_str(
            "2. `logs` {device, stream: true, last: \"1s\"} starts the log stream before the first step (iOS; Android has no log tool).\n",
        );
    }
    text.push_str("3. Do each step with `tap`, `swipe`, `type`, `key` and `button`, in device pixels from the latest screenshot");
    text.push_str(if shots {
        ", with a `screenshot` after each: there is no screen recording over MCP, so the screenshots in order are the recording.\n"
    } else {
        "; take a `screenshot` when you need to see where you are.\n"
    });
    text.push_str("4. When the bug shows, or the steps run out: ");
    if logs {
        text.push_str("`logs` {device, last: \"<seconds since step 1>s\"}, ");
    }
    text.push_str("a `screenshot`, and `crash_reports` {device} if the app quit (iOS).\n");
    text.push_str("Done when: you report whether it reproduced, the steps as you ran them");
    if shots {
        text.push_str(", the screenshots in order");
    }
    if logs {
        text.push_str(", the log lines that matter (errors, the app's own)");
    }
    text.push_str(", and anything that differed from the report.");
    text
}

fn audit_contrast(a: &Args) -> String {
    let d = device(a);
    let mut text = match a.get("screen_description") {
        Some(s) => format!(
            "Audit the screen \"{s}\" on {d} for WCAG contrast.\n\
             1. `screenshot` {{device}} to see where it is; get to that screen with `tap`, `swipe` and `button` \
             (device pixels), and `screenshot` again to check you are there.\n"
        ),
        None => format!("Audit the screen showing on {d} for WCAG contrast.\n1. `screenshot` {{device}} to see it.\n"),
    };
    text.push_str(
        "2. `contrast_check` {device, appearance: \"both\"}: on iOS it checks light and dark and puts the appearance back; \
         Android checks the one showing.\n\
         Done when: you list each finding, worst first: the appearance, what the element is, its box, the two colours, and its \
         ratio against the 4.5:1 (text) or 3:1 (large text, icons, controls) it needs. None: say so for each appearance.",
    );
    text
}

fn find_slowness(a: &Args) -> String {
    let d = a.req("device");
    format!(
        "Find what makes \"{d}\" slow.\n\
         1. `host`: the host's verdict (admit, hold or backoff) and its reasons: memory pressure, thermal state, load, swap.\n\
         2. `devices_usage`: this device's CPU and memory beside the other running devices and simee's own processes.\n\
         3. `processes` {{device: \"{d}\"}}: rank them by CPU and by memory.\n\
         4. For the app's process, if it is the heavy one (iOS): `mem_vmmap` {{device: \"{d}\", pid}} for its footprint by \
         region, `mem_heap` {{device: \"{d}\", pid}} for its objects by class, and `mem_leaks` {{device: \"{d}\", pid}} if its \
         memory keeps growing.\n\
         Done when: you name the cause with its numbers (the host's verdict and reason; the device's share against the others; or \
         a process's pid, name, CPU % and memory) and what to try next."
    )
}

fn across_form_factors(a: &Args) -> String {
    let app = a.req("app_path");
    let factors: Vec<&str> = a.req("form_factors").split(',').map(str::trim).filter(|f| !f.is_empty()).collect();
    format!(
        "Test {app} on each of: {}.\n\
         Its platform is the app's: .app is iOS, .apk or .aab Android.\n\
         1. `form_factors` {{platform}}: drop the form factors the platform doesn't have, and say why.\n\
         2. `devices` {{platform, form_factor}} for each: pick one device each, or `create` {{name, platform, form_factor}} where \
         there is none.\n\
         3. `boot_many` {{devices: [...], admission: \"wait\"}}: boots them as the host admits them (a task to poll, for a client \
         with tasks); the pacing in its result says when each started.\n\
         4. For each device: `install` {{device, path: \"{app}\"}}, `apps` {{device}} for its bundle id, `launch` {{device, \
         bundle_id}}, then `screenshot` {{device}}.\n\
         Done when: there is a screenshot per form factor, each with what is wrong in its layout (clipped, overlapping, \
         stretched, unused space) or \"fine\", and the form factors skipped with why. The devices stay booted: `shutdown` each \
         when the user is done with them.",
        factors.join(", ")
    )
}

/// The prompt's tools that `body` names, in the prompt's order.
fn named(prompt: &Prompt, body: &str) -> Vec<&'static str> {
    prompt.tools.iter().copied().filter(|t| body.contains(&format!("`{t}`"))).collect()
}

/// The toolset that has `tool`, as `SIMEE_MCP_TOOLSETS` spells it (a core tool's is `core`).
fn toolset(tool: &str) -> &'static str {
    if catalog::CORE.contains(&tool) { CORE } else { catalog::group(tool).map_or("all", |g| g.name()) }
}

/// What the prompt's tools need from this server: each toolset's tools, then those it doesn't serve
/// and the setting that adds them.
fn toolsets_note(tools: &[&str], served: &Toolsets) -> String {
    let mut sets: Vec<(&str, Vec<&str>)> = Vec::new();
    for tool in tools {
        let set = toolset(tool);
        match sets.iter_mut().find(|(s, _)| *s == set) {
            Some((_, list)) => list.push(tool),
            None => sets.push((set, vec![tool])),
        }
    }
    let quote = |list: &[&str]| list.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", ");
    let has: Vec<String> = sets.iter().map(|(set, list)| format!("{set} has {}", quote(list))).collect();
    let mut note = format!("Toolsets: {}.", has.join("; "));
    let missing: Vec<&str> = tools.iter().copied().filter(|t| !served.contains(t)).collect();
    if missing.is_empty() {
        note.push_str(" This server serves all of them.");
        return note;
    }
    let mut want = served.label();
    for tool in &missing {
        let set = toolset(tool);
        if !want.split(',').any(|s| s == set) {
            want.push(',');
            want.push_str(set);
        }
    }
    note.push_str(&format!(
        " This server serves {} and not {}: ask the user to restart simee mcp with {TOOLSETS_ENV}={want} (or all) first.",
        served.label(),
        quote(&missing)
    ));
    note
}

/// Whether the revision has titles on prompts and their arguments (2025-06-18 on).
fn titles(ctx: &Ctx) -> bool {
    ctx.version != "2025-03-26"
}

fn definition(prompt: &Prompt, ctx: &Ctx) -> Value {
    let arguments: Vec<Value> = prompt
        .arguments
        .iter()
        .map(|a| {
            let mut v = json!({"name": a.name, "description": a.description, "required": a.required});
            if titles(ctx) {
                v["title"] = a.title.into();
            }
            v
        })
        .collect();
    let mut v = json!({"name": prompt.name, "description": prompt.description, "arguments": arguments});
    if titles(ctx) {
        v["title"] = prompt.title.into();
    }
    v
}

/// How many prompts a `prompts/list` page holds.
pub const PAGE: usize = 50;

/// `prompts/list`: a page of the prompts, from the `cursor` a previous page gave.
pub fn list(params: &Value, ctx: &Ctx) -> Result<Value, RpcError> {
    list_page(params, ctx, PAGE)
}

fn list_page(params: &Value, ctx: &Ctx, size: usize) -> Result<Value, RpcError> {
    let start = match params.get("cursor") {
        None | Some(Value::Null) => 0,
        Some(Value::String(c)) => c
            .parse::<usize>()
            .ok()
            .filter(|n| (1..PROMPTS.len()).contains(n))
            .ok_or_else(|| RpcError::invalid_params(format!("Invalid cursor: {c:?} isn't a previous page's `nextCursor`")))?,
        Some(other) => return Err(RpcError::invalid_params(format!("`cursor` is a string, not {other}"))),
    };
    let end = start.saturating_add(size.max(1)).min(PROMPTS.len());
    let prompts: Vec<Value> = PROMPTS[start..end].iter().map(|p| definition(p, ctx)).collect();
    let mut result = json!({"prompts": prompts});
    if end < PROMPTS.len() {
        result["nextCursor"] = end.to_string().into();
    }
    Ok(result)
}

/// A prompt's arguments, checked against what it takes: strings only, none it doesn't take, every
/// required one there and not blank, and a value from its choices where it has them.
fn check(prompt: &Prompt, params: &Value) -> Result<Args, RpcError> {
    let args = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(o)) => o.clone(),
        Some(_) => return Err(RpcError::invalid_params("`arguments` must be an object of strings")),
    };
    let takes = || prompt.arguments.iter().map(|a| a.name).collect::<Vec<_>>().join(", ");
    for (key, value) in &args {
        let Some(a) = prompt.arguments.iter().find(|a| a.name == key) else {
            return Err(RpcError::invalid_params(format!("`{}` takes no argument `{key}` (it takes {})", prompt.name, takes())));
        };
        let Some(text) = value.as_str() else {
            return Err(RpcError::invalid_params(format!("`{key}` must be a string, not {value}")));
        };
        let text = text.trim();
        if !a.choices.is_empty() && !text.is_empty() && !a.choices.contains(&text) {
            return Err(RpcError::invalid_params(format!("`{key}` is one of {}, not {text:?}", a.choices.join(", "))));
        }
        if a.list && !text.is_empty() && text.split(',').all(|i| i.trim().is_empty()) {
            return Err(RpcError::invalid_params(format!("`{key}` names at least one, separated by commas")));
        }
    }
    let args = Args(args);
    let missing: Vec<&str> = prompt.arguments.iter().filter(|a| a.required && args.get(a.name).is_none()).map(|a| a.name).collect();
    if !missing.is_empty() {
        let list = missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ");
        return Err(RpcError::invalid_params(format!("`{}` needs {list}", prompt.name)).with_data(json!({"missing": missing})));
    }
    Ok(args)
}

/// `prompts/get`: the prompt's text for these arguments, with what its tools need from this server.
pub fn get(params: &Value, served: &Toolsets) -> Result<Value, RpcError> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
    let prompt = PROMPTS.iter().find(|p| p.name == name).ok_or_else(|| {
        let names = PROMPTS.iter().map(|p| p.name).collect::<Vec<_>>().join(", ");
        RpcError::invalid_params(format!("Unknown prompt: {name} (there are {names})"))
    })?;
    let args = check(prompt, params)?;
    let body = (prompt.render)(&args);
    let text = format!("{body}\n\n{}", toolsets_note(&named(prompt, &body), served));
    Ok(json!({"description": prompt.description, "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}))
}

#[cfg(test)]
mod tests;
