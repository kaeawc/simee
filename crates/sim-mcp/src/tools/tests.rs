use super::*;
use crate::backend::fake::Fake;

fn ctx(era: Era, version: &'static str) -> Ctx {
    Ctx { era, version, caps: json!({}), progress_token: None }
}

fn modern() -> Ctx {
    ctx(Era::Modern, crate::rpc::MODERN)
}

/// Every tool, for the current revision.
fn all() -> Vec<Value> {
    list(&Toolsets::all(), &modern())
}

fn tool(tools: &[Value], name: &str) -> Value {
    tools.iter().find(|t| t["name"] == name).cloned().unwrap_or_else(|| panic!("no tool {name}"))
}

fn run_plain(backend: &dyn Backend, name: &str, args: &Value) -> Result<Value, String> {
    run(backend, &Pages::default(), "me", name, args)
}

/// Whether `v` fits `schema` (type, enum, properties, required, items, additionalProperties,
/// maxProperties, anyOf): the path to the first misfit.
fn misfit(schema: &Value, v: &Value, at: &str) -> Option<String> {
    if let Some(alts) = schema["anyOf"].as_array()
        && !alts.iter().any(|a| misfit(a, v, at).is_none())
    {
        return Some(format!("{at}: none of anyOf fits {v}"));
    }
    let kind = match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    let types: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !types.is_empty() && !types.contains(&kind) {
        return Some(format!("{at}: {kind} isn't {types:?}"));
    }
    match v {
        Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| misfit(&schema["items"], item, &format!("{at}[{i}]"))),
        Value::Object(fields) => {
            if schema["maxProperties"].as_u64().is_some_and(|n| fields.len() as u64 > n) {
                return Some(format!("{at}: more than {} keys", schema["maxProperties"]));
            }
            if schema["additionalProperties"] == false
                && let Some(k) = fields.keys().find(|k| schema["properties"].get(k.as_str()).is_none())
            {
                return Some(format!("{at}: `{k}` isn't allowed"));
            }
            if let Some(k) =
                schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|k| !fields.contains_key(*k))
            {
                return Some(format!("{at}: no `{k}`"));
            }
            fields.iter().find_map(|(k, field)| {
                let sub = schema["properties"].get(k).or_else(|| schema.get("additionalProperties").filter(|a| a.is_object()))?;
                misfit(sub, field, &format!("{at}.{k}"))
            })
        }
        _ => None,
    }
}

fn admission() -> Value {
    json!({"verdict": "admit", "reasons": ["1 more iOS phone device would take memory used from 30% to 33%"],
        "predicted": {"subject": "1 more iOS phone device", "devices": 1, "admissible": 1, "costs": [{"platform": "ios", "count": 1}],
        "boot_cpu_pct": 280.0, "idle_cpu_pct": 10.0, "boot_memory_bytes": 2176000000u64, "idle_memory_bytes": 2176000000u64,
        "memory_used_pct": {"now": 30.0, "after": 33.0}, "memory_available_bytes": null, "load": {"now": 1.0, "after": 3.8, "limit": 24.0}}})
}

#[test]
fn a_lifecycle_result_carries_its_admission_in_structured_content_and_one_line_of_text() {
    let text = |name: &str, value: Value| result(name, Ok(value), Era::Modern)["content"][0]["text"].as_str().expect("text").to_owned();
    // The verdict once as data, a few words as text: not the whole JSON twice (#292).
    let out = result("boot", Ok(json!({"admission": admission()})), Era::Modern);
    assert_eq!(out["content"][0]["text"], "ok (admit)");
    assert_eq!(out["structuredContent"]["admission"]["verdict"], "admit");
    // Timings stay in the text, without the admission and pacing.
    let timed = json!({"device": "d", "boot_s": 1.5, "was_booted": false, "admission": admission(), "pacing": {"steps": []}});
    let t = text("boot", timed.clone());
    assert_eq!(t, r#"{"boot_s":1.5,"device":"d","was_booted":false} (admit)"#);
    assert_eq!(result("boot", Ok(timed.clone()), Era::Modern)["structuredContent"], timed);
    let held = admission().as_object().cloned().map(|mut a| {
        a.insert("verdict".into(), json!("hold"));
        Value::Object(a)
    });
    assert_eq!(text("create", json!({"udid": "u", "admission": held})), r#"{"udid":"u"} (hold)"#);
    // What isn't a lifecycle result is as it was; admission_check's text is its answer.
    assert_eq!(text("boot", Value::Null), "ok");
    assert_eq!(text("devices", json!([])), "[]");
    assert!(text("admission_check", admission()).contains("\"reasons\""));
}

#[test]
fn there_is_a_tool_for_every_method_in_the_table() {
    let tools = all();
    for (method, params, doc) in METHODS {
        let t = tool(&tools, method);
        let description = t["description"].as_str().expect("description");
        if withheld(method).is_none() {
            assert!(description.starts_with(doc), "{method}: the table's own text comes first");
            assert!(params.is_empty() || description.contains(params), "{method} lists its parameters");
        }
        assert!(description.contains("Platforms: "), "{method} says where it works");
        assert!(description.contains("Toolset: ") || description.contains("Toolsets: core, "), "{method} says its toolset");
        assert_eq!(t["inputSchema"]["type"], "object", "{method}");
        assert!(t["outputSchema"].is_object(), "{method} has an output schema");
        assert!(t["annotations"]["readOnlyHint"].is_boolean(), "{method} has annotations");
    }
    assert_eq!(tools.len(), METHODS.len() + 1, "the table plus the view");
    assert_eq!(tools.last().map(|t| t["name"].clone()), Some(json!(VIEW_TOOL)));
    // Tool names are what the spec allows, and unique.
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.iter().all(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')));
    names.sort();
    names.dedup();
    assert_eq!(names.len(), tools.len());
}

#[test]
fn descriptions_say_what_a_method_does_where_and_in_what_units() {
    let tools = all();
    let find = |m: &str| tool(&tools, m)["description"].as_str().expect(m).to_owned();
    assert!(find("tap").contains("Platforms: Android, iOS."));
    assert!(find("xr").contains("Platforms: Android."));
    assert!(!find("xr").contains("iOS."));
    assert!(find("fold").contains("Platforms: Android, iOS."), "the iPhone Duo folds too");
    assert!(find("status_bar").contains("Platforms: iOS."));
    assert!(find("boot").contains("task") && find("boot").contains("Can take a while"));
    assert!(!find("tap").contains("task"));
    // Coordinates in device pixels where the method takes them; not for the touchpad's own units.
    for m in ["tap", "swipe", "touch", "stylus", "pointer"] {
        assert!(find(m).contains("device pixels"), "{m}");
        let schema = tool(&tools, m)["inputSchema"].clone();
        let x = if m == "swipe" { "x1" } else { "x" };
        assert!(schema["properties"][x]["description"].as_str().is_some_and(|d| d.contains("device pixels")), "{m}");
    }
    assert!(!find("touchpad").contains("device pixels") && !find("xr").contains("device pixels"));
    // What it does to the device, from the catalog.
    assert!(find("devices").contains("Read-only"));
    assert!(find("erase").contains("Destructive"));
    assert!(find("exec").contains("Caution"));
    assert!(find("logs").contains("next_cursor") && !find("tap").contains("cursor"));
    assert!(find(VIEW_TOOL).contains("Platforms: Android, iOS.") && find(VIEW_TOOL).contains("Toolsets: core, observe."));
    // Units from parameter names.
    let boot = tool(&tools, "boot")["inputSchema"].clone();
    assert!(boot["properties"]["timeout_s"]["description"].as_str().is_some_and(|d| d.contains("seconds")));
    assert!(boot["properties"]["settle_cpu_pct"]["description"].as_str().is_some_and(|d| d.contains("percent of one core")));
    assert!(tool(&tools, "tap")["inputSchema"]["properties"]["hold_ms"]["description"] == "milliseconds");
}

#[test]
fn annotations_follow_what_a_method_does() {
    let tools = all();
    let ann = |m: &str| tool(&tools, m)["annotations"].clone();
    assert_eq!(ann("devices")["readOnlyHint"], true);
    assert_eq!(ann("devices").get("destructiveHint"), None);
    assert_eq!((ann("delete")["destructiveHint"].clone(), ann("delete")["idempotentHint"].clone()), (json!(true), json!(true)));
    assert_eq!((ann("tap")["readOnlyHint"].clone(), ann("tap")["destructiveHint"].clone()), (json!(false), json!(false)));
    assert_eq!(ann("tap")["idempotentHint"], false);
    assert_eq!(ann("appearance")["idempotentHint"], true);
    assert_eq!(ann("tap")["openWorldHint"], false);
    assert_eq!(ann("open_url")["openWorldHint"], true, "a URL the device opens may be anywhere");
    // Read-only means it changes nothing: not a capture that stops and writes a recording.
    assert_eq!(ann("audio_capture_stop")["readOnlyHint"], false);
    assert_eq!(ann("screenshot")["readOnlyHint"], true);
    assert_eq!(ann(VIEW_TOOL)["readOnlyHint"], true);
    // A method nobody classified gets the specification's defaults, not a claim of safety.
    let unknown = annotations("a_new_method");
    assert_eq!(unknown["readOnlyHint"], false);
    assert_eq!((unknown.get("destructiveHint"), unknown.get("idempotentHint")), (None, None));
}

#[test]
fn every_destructive_method_is_annotated_destructive() {
    let tools = all();
    let mut destructive = Vec::new();
    for (method, ..) in METHODS {
        let e = catalog::effect(method).unwrap_or_else(|| panic!("{method} is classified in sim-api's catalog"));
        let a = tool(&tools, method)["annotations"].clone();
        assert_eq!(a["readOnlyHint"], e.read_only(), "{method}");
        if e.read_only() {
            assert_eq!((a.get("destructiveHint"), a.get("idempotentHint")), (None, None), "{method}");
        } else {
            assert_eq!(a["destructiveHint"], e.destructive(), "{method}");
            assert_eq!(a["idempotentHint"], e.idempotent(), "{method}");
        }
        if e.destructive() {
            destructive.push(*method);
        }
    }
    // What deletes, erases, kills, clears data, resets the keychain, shuts down, uninstalls or
    // removes a preference, and what runs code or overwrites files.
    for m in [
        "delete",
        "erase",
        "kill",
        "clear_container",
        "reset_keychain",
        "shutdown",
        "uninstall",
        "terminate",
        "prefs_delete",
        "prefs_write",
        "prefs_batch",
        "rm",
        "write",
        "defaults_write",
        "exec",
        "launch",
        "push",
        "network_shape",
        "console",
        "audio_capture_start",
        "export",
    ] {
        assert!(destructive.contains(&m), "{m} is marked destructive");
    }
}

#[test]
fn boot_manys_schema_declares_a_cancelled_result_for_every_era() {
    for era in [Era::Modern, Era::Legacy] {
        let schema = output_schema("boot_many", era);
        assert_eq!(schema["properties"]["cancelled"]["type"], "boolean", "{era:?}: {schema}");
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.contains(&json!("cancelled")), "only a cancelled call has it ({era:?})");
    }
}

#[test]
fn every_tool_has_an_output_schema_its_results_fit() {
    for (era, version) in [(Era::Modern, crate::rpc::MODERN), (Era::Legacy, "2025-11-25"), (Era::Legacy, "2025-06-18")] {
        let tools = list(&Toolsets::all(), &ctx(era, version));
        for t in &tools {
            let schema = &t["outputSchema"];
            assert!(schema.is_object(), "{} for {version}", t["name"]);
            // A schema keyword is never null (a misspelt key read mutably would leave one).
            fn has_null(v: &Value) -> bool {
                match v {
                    Value::Null => true,
                    Value::Array(a) => a.iter().any(has_null),
                    Value::Object(o) => o.iter().any(|(k, v)| k != "enum" && has_null(v)),
                    _ => false,
                }
            }
            assert!(!has_null(schema) && !has_null(&t["inputSchema"]), "{} has a null in a schema: {t}", t["name"]);
            if era == Era::Legacy {
                assert_eq!(schema["type"], "object", "{}: an older revision's output schema is an object", t["name"]);
            }
        }
    }
    // 2025-03-26 has no output schemas.
    let old = list(&Toolsets::all(), &ctx(Era::Legacy, "2025-03-26"));
    assert!(old.iter().all(|t| t.get("outputSchema").is_none()));
    // The structured content of a result fits its tool's schema, in both shapes.
    let cases = [
        ("tap", Value::Null),
        (
            "devices",
            json!([{"udid": "a", "name": "n", "state": "Booted", "runtime_id": "r", "device_type_id": "t", "available": true,
            "platform": "ios", "form_factor": "phone", "sub_platform": null}]),
        ),
        ("logs", json!(["a line"])),
        ("defaults_read", json!("{ key = 1; }")),
        ("rotate", Value::Null),
        ("rotate", json!({"orientation": "portrait"})),
        ("carplay", Value::Null),
        ("vehicle", Value::Null),
        ("vehicle", json!([{"property": "GEAR_SELECTION", "id": "0x11400400", "area_id": 0, "status": "available", "value": 4}])),
        ("boot", Value::Null),
        ("boot", json!({"device": "d", "boot_s": 1.0, "was_booted": false})),
        ("boot", json!({"admission": admission(), "pacing": {"waited_s": 5.0, "timed_out": false, "cancelled": false, "steps": []}})),
        ("boot", json!({"device": "d", "boot_s": 1.0, "was_booted": false, "admission": admission()})),
        (
            "create",
            json!({"udid": "u", "name": "n", "state": "Shutdown", "runtime_id": "r", "device_type_id": "t", "available": true,
            "platform": "ios", "form_factor": "phone", "sub_platform": null, "admission": admission()}),
        ),
        ("admission_check", admission()),
        ("screen", json!({"width": 2, "height": 1})),
        ("screenshot", json!({"width": 2, "height": 1, "png": "iVBOR"})),
        (
            "contrast_check",
            json!({"png": "CCC", "findings": [], "failing": 0, "checked": 0, "width": 2, "height": 1, "scale": 1.0,
            "scale_source": "assumed"}),
        ),
        ("network_env", json!({"DYLD_INSERT_LIBRARIES": "/x"})),
        (VIEW_TOOL, json!({"device": "booted", "screen": {"width": 1, "height": 2}})),
        ("processes", json!({"items": [], "next_cursor": null, "total": 0})),
    ];
    for era in [Era::Modern, Era::Legacy] {
        for (name, value) in &cases {
            let out = result(name, Ok(value.clone()), era);
            let schema = output_schema(name, era);
            let content = &out["structuredContent"];
            assert!(!content.is_null(), "{name}: every result has structured content");
            assert_eq!(misfit(&schema, content, name), None, "{name} {era:?}: {content} against {schema}");
        }
    }
    // The schemas say something: a wrong shape doesn't fit.
    assert!(misfit(&output_schema("tap", Era::Modern), &json!({"x": 1}), "tap").is_some());
    assert!(misfit(&output_schema("logs", Era::Modern), &json!({"x": 1}), "logs").is_some());
    assert!(misfit(&output_schema("logs", Era::Legacy), &json!({"result": "x"}), "logs").is_some());
}

#[test]
fn screenshot_does_not_take_a_host_path() {
    let tools = all();
    let shot = tool(&tools, "screenshot");
    assert_eq!(shot["inputSchema"]["properties"].get("path"), None);
    assert!(!shot["description"].as_str().expect("description").contains("path"));
    assert!(shot["inputSchema"]["properties"]["device"].is_object());
    assert_eq!(shot["outputSchema"]["properties"].get("png"), None, "the PNG is an image block");
    let fake = Fake::default();
    let err = run_plain(&fake, "screenshot", &json!({"device": "d", "path": "/home/me/.zshrc"})).expect_err("refused");
    assert!(err.contains("`path` isn't available through MCP"), "{err}");
    assert!(fake.called("screenshot").is_empty(), "never reaches the API");
    assert!(run_plain(&fake, "screenshot", &json!({"device": "d", "path": null})).is_ok());
    // Other methods keep their parameters.
    assert!(tool(&tools, "audio_capture_start")["inputSchema"]["properties"].get("path").is_some());
    assert_eq!(tool(&tools, "contrast_check")["outputSchema"]["properties"].get("path"), None);
}

#[test]
fn the_docs_name_every_tool_and_resource() {
    // docs/mcp.md lives at the repository root, outside the published crate: in the
    // workspace a missing doc fails, from an unpacked .crate the test has nothing to check.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join("crates/sim-mcp/Cargo.toml").is_file() {
        eprintln!("skipped: not in the simee workspace, so docs/mcp.md isn't here");
        return;
    }
    let docs = std::fs::read_to_string(root.join("docs/mcp.md")).expect("docs/mcp.md");
    for tool in all() {
        let name = tool["name"].as_str().expect("name");
        assert!(docs.contains(&format!("`{name}`")), "docs/mcp.md doesn't name the tool `{name}`: add it to the tool table");
    }
    for uri in
        [crate::resources::DEVICES, crate::resources::RUNTIMES, crate::resources::DEVICE_TYPES, crate::resources::FORM_FACTORS, app::URI]
            .into_iter()
            .map(str::to_owned)
            .chain(
                crate::resources::templates(&Toolsets::all())
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t["uriTemplate"].as_str().map(str::to_owned)),
            )
    {
        assert!(docs.contains(&format!("`{uri}`")), "docs/mcp.md doesn't name the resource `{uri}`");
    }
    for method in LONG_RUNNING {
        assert!(docs.contains(&format!("`{method}`")), "{method}");
    }
    // Each toolset, and the default one's tools.
    for name in Toolsets::names().split(", ") {
        assert!(docs.contains(&format!("`{name}`")), "docs/mcp.md doesn't name the toolset `{name}`");
    }
    assert!(docs.contains(TOOLSETS_ENV));
}

#[test]
fn the_view_tool_points_at_the_app() {
    let tools = all();
    let view = tools.last().expect("view");
    assert_eq!(view["_meta"]["ui"]["resourceUri"], app::URI);
    assert!(tools.iter().filter(|t| t.get("_meta").is_some()).count() == 1);
}

#[test]
fn arguments_are_objects() {
    let every = Toolsets::all();
    assert_eq!(arguments(&json!({"name": "tap"})), Ok(json!({})));
    assert_eq!(arguments(&json!({"arguments": null})), Ok(json!({})));
    assert_eq!(arguments(&json!({"arguments": {"x": 1}})), Ok(json!({"x": 1})));
    assert_eq!(arguments(&json!({"arguments": [1]})).expect_err("array").code, crate::rpc::INVALID_PARAMS);
    assert_eq!(parse_call(&json!({"name": "nope"}), &every).expect_err("unknown").message, "Unknown tool: nope");
    assert!(parse_call(&json!({}), &every).is_err());
    assert_eq!(parse_call(&json!({"name": "tap", "arguments": {"x": 1}}), &every), Ok(("tap".to_owned(), json!({"x": 1}))));
}

#[test]
fn toolsets_pick_the_tools_and_a_hidden_one_says_where_it_is() {
    let core = list(&Toolsets::default(), &modern());
    let names: Vec<&str> = core.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.len() <= 25, "the default toolset is small: {} tools", names.len());
    for m in ["devices", "boot", "screenshot", "tap", "swipe", "type", "button", "launch", "logs", VIEW_TOOL] {
        assert!(names.contains(&m), "{m} is in core");
    }
    assert!(!names.contains(&"exec") && !names.contains(&"xr"));
    // Picking adds groups; `all` is everything; order and spelling are forgiving, unknown names aren't.
    let picked = Toolsets::parse("core, debug").expect("core,debug");
    let with_debug = list(&picked, &modern());
    assert_eq!(with_debug.len(), core.len() + METHODS.iter().filter(|(m, ..)| catalog::group(m) == Some(Group::Debug)).count());
    assert!(with_debug.iter().any(|t| t["name"] == "exec"));
    assert_eq!(picked.label(), "core,debug");
    let prefs = list(&Toolsets::parse("prefs").expect("prefs"), &modern());
    assert!(prefs.iter().all(|t| t["name"].as_str().is_some_and(|n| n.starts_with("prefs_"))) && prefs.len() == 6);
    assert_eq!(Toolsets::parse("all").expect("all"), Toolsets::all());
    assert_eq!(Toolsets::parse("").expect("empty"), Toolsets::default());
    assert_eq!(Toolsets::all().label(), "all");
    assert_eq!(list(&Toolsets::all(), &modern()).len(), METHODS.len() + 1);
    for bad in ["all,bogus", "bogus,all"] {
        assert!(Toolsets::parse(bad).expect_err(bad).contains("unknown toolset `bogus`"), "{bad}");
    }
    let err = Toolsets::parse("core,streaming").expect_err("unknown");
    assert!(err.contains("unknown toolset `streaming`") && err.contains("debug") && err.contains(TOOLSETS_ENV), "{err}");
    // Every tool is in one group, so `all` is every group.
    for (method, ..) in METHODS {
        assert_eq!(Group::ALL.iter().filter(|g| catalog::group(method) == Some(**g)).count(), 1, "{method}");
    }
    // A call to a tool the server doesn't serve is refused with the toolset that has it.
    let err = parse_call(&json!({"name": "exec", "arguments": {}}), &Toolsets::default()).expect_err("hidden");
    assert_eq!(err.code, crate::rpc::INVALID_PARAMS);
    assert!(err.message.contains("`exec` isn't enabled") && err.message.contains("`debug` toolset"), "{}", err.message);
    assert!(err.message.contains("SIMEE_MCP_TOOLSETS=core,debug"), "{}", err.message);
    assert_eq!(err.data, Some(json!({"tool": "exec", "toolset": "debug", "enabled": "core"})));
    assert!(parse_call(&json!({"name": "exec"}), &picked).is_ok());
    // `stream_start` with `input: true` lets viewers touch the device: it needs the input tools.
    let streaming = Toolsets::parse("stream").expect("stream");
    let input = json!({"name": "stream_start", "arguments": {"whip_url": "http://127.0.0.1:1/w", "input": true}});
    let err = parse_call(&input, &streaming).expect_err("input needs the input toolset");
    assert!(err.message.contains("`input` toolset") && err.message.contains("SIMEE_MCP_TOOLSETS=stream,input"), "{}", err.message);
    assert!(parse_call(&input, &Toolsets::parse("stream,input").expect("both")).is_ok());
    assert!(parse_call(&json!({"name": "stream_start", "arguments": {"whip_url": "http://127.0.0.1:1/w"}}), &streaming).is_ok());
    assert_eq!(parse_call(&json!({"name": "teleport"}), &Toolsets::default()).expect_err("unknown").message, "Unknown tool: teleport");
}

/// The default toolset's `tools/list`, pinned: a change to a description, schema or annotation of
/// the tools most agents see shows up here. `SIMEE_MCP_UPDATE_SNAPSHOT=1` rewrites it.
#[test]
fn the_default_tools_list_is_pinned() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots/tools-list-core.json");
    let now = serde_json::to_string_pretty(&json!({"tools": list(&Toolsets::default(), &modern())})).expect("json") + "\n";
    if std::env::var_os("SIMEE_MCP_UPDATE_SNAPSHOT").is_some() {
        std::fs::write(&path, &now).expect("write the snapshot");
        return;
    }
    let pinned = std::fs::read_to_string(&path).expect("src/snapshots/tools-list-core.json (SIMEE_MCP_UPDATE_SNAPSHOT=1 writes it)");
    assert!(pinned == now, "the default tools/list changed: rerun with SIMEE_MCP_UPDATE_SNAPSHOT=1 and review the diff");
}

#[test]
fn results_are_content_and_structure() {
    let ok = result("devices", Ok(json!([{"udid": "a"}])), Era::Modern);
    assert_eq!(ok["content"][0]["text"], r#"[{"udid":"a"}]"#);
    assert_eq!(ok["structuredContent"], json!([{"udid": "a"}]));
    // An older revision's structuredContent is an object.
    let old = result("devices", Ok(json!([1])), Era::Legacy);
    assert_eq!(old["structuredContent"], json!({"result": [1]}));
    assert_eq!(result("info", Ok(json!({"a": 1})), Era::Legacy)["structuredContent"], json!({"a": 1}));
    // Nothing to report is `ok`, and an empty object.
    let none = result("tap", Ok(Value::Null), Era::Modern);
    assert_eq!((none["content"][0]["text"].clone(), none["structuredContent"].clone()), (json!("ok"), json!({})));
    let failed = result("tap", Err("no device matches `x`".into()), Era::Modern);
    assert_eq!((failed["isError"].clone(), failed["content"][0]["text"].clone()), (json!(true), json!("no device matches `x`")));
    assert_eq!(failed.get("structuredContent"), None, "an error has no structure");
    let shot = result("screenshot", Ok(json!({"width": 2, "height": 1, "png": "iVBOR"})), Era::Modern);
    assert_eq!(shot["content"][0], json!({"type": "image", "data": "iVBOR", "mimeType": "image/png"}));
    assert_eq!(shot["structuredContent"], json!({"width": 2, "height": 1}));
    // A contrast check's PNGs become image blocks and leave the text a few lines of findings.
    let check = result(
        "contrast_check",
        Ok(json!({"failing": 1, "appearances": {"light": {"png": "AAA", "findings": []}, "dark": {"png": "BBB", "findings": []}}})),
        Era::Modern,
    );
    // `image` is a result's place among the image blocks (JSON objects have no order to lean on).
    let at = |mode: &str| check["structuredContent"]["appearances"][mode]["image"].as_u64().expect("image") as usize + 1;
    assert_eq!(check["content"][at("light")], json!({"type": "image", "data": "AAA", "mimeType": "image/png"}));
    assert_eq!(check["content"][at("dark")]["data"], "BBB");
    assert_ne!(at("light"), at("dark"));
    assert!(!check["content"][0]["text"].as_str().expect("text").contains("AAA"));
    let single = result("contrast_check", Ok(json!({"png": "CCC", "findings": []})), Era::Modern);
    assert_eq!((single["content"][1]["data"].clone(), single["structuredContent"]["image_of"].clone()), (json!("CCC"), json!("screen")));
    // Written to a path, a screenshot is plain data.
    let path = result("screenshot", Ok(json!({"path": "/tmp/x.png", "width": 2, "height": 1})), Era::Modern);
    assert_eq!(path["structuredContent"]["path"], "/tmp/x.png");
}

#[test]
fn running_a_tool_calls_the_method_or_reads_the_screen_for_the_view() {
    let fake = Fake::default();
    fake.answer("screen", Ok(json!({"width": 1080, "height": 2400})));
    assert_eq!(run_plain(&fake, "tap", &json!({"device": "d", "x": 1, "y": 2})), Ok(Value::Null));
    assert_eq!(fake.called("tap"), [json!({"device": "d", "x": 1, "y": 2})]);
    let view = run_plain(&fake, VIEW_TOOL, &json!({})).expect("view");
    assert_eq!(view, json!({"device": "booted", "screen": {"width": 1080, "height": 2400}}));
    assert_eq!(fake.called("screen"), [json!({"device": "booted"})]);
    let shown = result(VIEW_TOOL, Ok(view), Era::Modern);
    assert!(shown["content"][0]["text"].as_str().is_some_and(|t| t.contains("1080")));
    assert_eq!(shown["structuredContent"]["device"], "booted");
    fake.answer("screen", Err("booted: no such device".into()));
    assert!(run_plain(&fake, VIEW_TOOL, &json!({"device": "x"})).is_err());
}

#[test]
fn long_lists_page_through_one_snapshot() {
    let fake = Fake::default();
    let procs: Vec<Value> = (1..=5).map(|pid| json!({"pid": pid, "name": format!("p{pid}")})).collect();
    fake.answer("processes", Ok(Value::Array(procs.clone())));
    let pages = Pages::default();
    // Without `limit` or `cursor`: the whole list, as the API returns it.
    assert_eq!(run(&fake, &pages, "me", "processes", &json!({"device": "d"})), Ok(Value::Array(procs.clone())));
    let first = run(&fake, &pages, "me", "processes", &json!({"device": "d", "limit": 2})).expect("page 1");
    assert_eq!((first["items"].clone(), first["total"].clone()), (json!(procs[..2]), json!(5)));
    assert_eq!(fake.called("processes").last(), Some(&json!({"device": "d"})), "paging isn't the API's");
    // The next pages come from the snapshot, even after the list changed.
    fake.answer("processes", Ok(json!([])));
    let cursor = first["next_cursor"].clone();
    let second = run(&fake, &pages, "me", "processes", &json!({"device": "d", "cursor": cursor})).expect("page 2");
    assert_eq!(second["items"], json!(procs[2..4]));
    let third = run(&fake, &pages, "me", "processes", &json!({"cursor": second["next_cursor"], "limit": 10})).expect("page 3");
    assert_eq!((third["items"].clone(), third["next_cursor"].clone()), (json!(procs[4..]), Value::Null));
    assert_eq!(fake.called("processes").len(), 2, "one call for the three pages");
    // A cursor this server didn't hand out, or for another tool, is refused.
    for (tool, cursor) in [("processes", json!("99.0.2")), ("processes", json!("junk")), ("logs", cursor.clone())] {
        let err = run(&fake, &pages, "me", tool, &json!({"cursor": cursor})).expect_err("expired");
        assert!(err.contains("`cursor` has expired"), "{err}");
    }
    for limit in [json!(0), json!(MAX_LIMIT + 1), json!("2")] {
        assert!(run(&fake, &pages, "me", "processes", &json!({"limit": limit})).expect_err("limit").contains("`limit`"));
    }
    // A list inside an object pages in place.
    fake.answer("devices_usage", Ok(json!({"available": true, "devices": [{"id": "a"}, {"id": "b"}, {"id": "c"}], "simee": {}})));
    let usage = run(&fake, &pages, "me", "devices_usage", &json!({"limit": 2})).expect("usage");
    assert_eq!(
        (usage["devices"].clone(), usage["total"].clone(), usage["available"].clone()),
        (json!([{"id": "a"}, {"id": "b"}]), json!(3), json!(true))
    );
    let rest = run(&fake, &pages, "me", "devices_usage", &json!({"cursor": usage["next_cursor"]})).expect("rest");
    assert_eq!((rest["devices"].clone(), rest["next_cursor"].clone()), (json!([{"id": "c"}]), Value::Null));
    // A short list is one page, and keeps nothing.
    fake.answer("logs", Ok(json!(["one"])));
    assert_eq!(run(&fake, &pages, "me", "logs", &json!({"limit": 5})), Ok(json!({"items": ["one"], "next_cursor": null, "total": 1})));
    // Tools that don't page pass `limit` through to the method, as before.
    assert!(run(&fake, &pages, "me", "apps", &json!({"limit": 1})).is_ok());
    assert_eq!(fake.called("apps"), [json!({"limit": 1})]);
    let listed = all();
    assert_eq!(tool(&listed, "logs")["inputSchema"]["properties"]["limit"]["maximum"], MAX_LIMIT);
    assert!(tool(&listed, "apps")["inputSchema"]["properties"].get("cursor").is_none());
}

#[test]
fn only_long_methods_for_task_clients_become_tasks() {
    let caps = json!({"extensions": {crate::rpc::EXT_TASKS: {}}});
    let tasks = Ctx { era: Era::Modern, version: crate::rpc::MODERN, caps, progress_token: None };
    let plain = ctx(Era::Modern, crate::rpc::MODERN);
    let legacy = Ctx { era: Era::Legacy, version: "2025-11-25", caps: tasks.caps.clone(), progress_token: None };
    assert!(as_task("boot", &tasks) && as_task("install", &tasks));
    assert!(!as_task("tap", &tasks) && !as_task("boot", &plain) && !as_task("boot", &legacy));
    for name in LONG_RUNNING {
        assert!(METHODS.iter().any(|(m, ..)| m == name), "{name} is not a method");
    }
}

#[test]
fn snapshots_are_bounded_and_a_crafted_cursor_is_refused() {
    let pages = Pages::default();
    let v = json!([1, 2, 3]);
    let none = json!({});
    let ids: Vec<String> = (0..6).map(|_| pages.keep_within("me", "processes", &none, &v, 3, usize::MAX)).collect();
    assert!(pages.find("me", "processes", &ids[0]).is_none() && pages.find("me", "processes", &ids[2]).is_none());
    assert!(pages.find("me", "processes", &ids[3]).is_some() && pages.find("me", "processes", &ids[5]).is_some());
    // By bytes: each is 7 bytes of JSON, so 15 holds two.
    let by_bytes = Pages::default();
    let ids: Vec<String> = (0..4).map(|_| by_bytes.keep_within("me", "processes", &none, &v, 16, 15)).collect();
    assert!(by_bytes.find("me", "processes", &ids[1]).is_none() && by_bytes.find("me", "processes", &ids[2]).is_some());
    assert!(by_bytes.find("me", "processes", &ids[3]).is_some());
    // One alone over the cap is still kept.
    let alone = by_bytes.keep_within("me", "processes", &none, &v, 16, 1);
    assert!(by_bytes.find("me", "processes", &alone).is_some());
    // A hand-made cursor with a page size out of range is refused, not looped on.
    let fake = Fake::default();
    let id = pages.keep_within("me", "processes", &none, &json!([1, 2, 3]), 3, usize::MAX);
    for size in [0, MAX_LIMIT + 1] {
        let err = run(&fake, &pages, "me", "processes", &json!({"cursor": format!("{id}.0.{size}")})).expect_err("range");
        assert!(err.contains("`cursor`"), "{err}");
    }
}

#[test]
fn a_cursor_works_for_the_client_it_was_given_to_and_nobody_else() {
    let fake = Fake::default();
    let procs: Vec<Value> = (1..=5).map(|pid| json!({"pid": pid})).collect();
    fake.answer("processes", Ok(Value::Array(procs.clone())));
    let pages = Pages::default();
    let first = run(&fake, &pages, "alice", "processes", &json!({"device": "d", "limit": 2})).expect("page 1");
    let cursor = first["next_cursor"].as_str().expect("cursor").to_owned();
    // Not a counter a neighbour can step through: 32 hex digits, different every time.
    let id = cursor.split('.').next().expect("id");
    assert!(id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()), "{cursor}");
    let again = run(&fake, &pages, "alice", "processes", &json!({"device": "d", "limit": 2})).expect("again");
    assert_ne!(again["next_cursor"], first["next_cursor"]);
    // Another client, holding alice's cursor (an HTTP server's stateless sessions share the snapshots):
    // the cursor is no cursor of theirs.
    let err = run(&fake, &pages, "mallory", "processes", &json!({"cursor": cursor})).expect_err("not theirs");
    assert!(err.contains("`cursor` has expired"), "{err}");
    assert!(run(&fake, &pages, "alice", "processes", &json!({"cursor": cursor})).is_ok());
}

/// A backend that allows a call only while `allowed` is true: the policy's, as paging sees it.
struct Revocable {
    inner: Fake,
    allowed: std::sync::atomic::AtomicBool,
    asked: Mutex<Vec<(String, Value)>>,
}

impl Backend for Revocable {
    fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
        self.inner.call(method, params)
    }

    fn permits(&self, method: &str, params: &Value) -> Result<(), String> {
        self.asked.lock().expect("asked").push((method.to_owned(), params.clone()));
        if self.allowed.load(std::sync::atomic::Ordering::SeqCst) { Ok(()) } else { Err("refused: not any more".into()) }
    }
}

#[test]
fn the_next_page_asks_the_policy_again_about_the_call_that_made_the_list() {
    let backend = Revocable { inner: Fake::default(), allowed: true.into(), asked: Mutex::default() };
    backend.inner.answer("processes", Ok(json!([1, 2, 3, 4, 5])));
    let pages = Pages::default();
    let first = run(&backend, &pages, "me", "processes", &json!({"device": "ci-1", "limit": 2})).expect("page 1");
    assert!(backend.asked.lock().expect("asked").is_empty(), "the first page is the call itself, which was judged as it ran");
    let cursor = first["next_cursor"].clone();
    // The page after it is judged by the call that made the list, not by what the cursor request says.
    run(&backend, &pages, "me", "processes", &json!({"device": "ci-2", "cursor": cursor})).expect("page 2");
    assert_eq!(*backend.asked.lock().expect("asked"), [("processes".to_owned(), json!({"device": "ci-1"}))]);
    backend.allowed.store(false, std::sync::atomic::Ordering::SeqCst);
    let err = run(&backend, &pages, "me", "processes", &json!({"cursor": cursor})).expect_err("revoked");
    assert!(err.starts_with("refused: "), "{err}");
}
