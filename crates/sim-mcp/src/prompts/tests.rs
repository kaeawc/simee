use std::collections::BTreeSet;

use super::*;
use crate::rpc::{Era, INVALID_PARAMS, LEGACY, MODERN};
use crate::tools;

fn ctx(version: &'static str) -> Ctx {
    let era = if version == MODERN { Era::Modern } else { Era::Legacy };
    Ctx { era, version, caps: json!({}), progress_token: None }
}

/// Every set of arguments worth rendering a prompt with: its required ones, with each optional one
/// given or not and each choice taken.
fn variants(prompt: &Prompt) -> Vec<Value> {
    let mut out = vec![json!({})];
    for a in prompt.arguments {
        let values: Vec<Option<&str>> = if !a.choices.is_empty() {
            a.choices.iter().copied().map(Some).collect()
        } else if a.list {
            vec![Some("phone, tablet")]
        } else {
            vec![Some("x")]
        };
        let values = if a.required { values } else { values.into_iter().chain([None]).collect() };
        out = out
            .iter()
            .flat_map(|base| {
                values.iter().map(move |v| {
                    let mut next = base.clone();
                    if let Some(v) = v {
                        next[a.name] = (*v).into();
                    }
                    next
                })
            })
            .collect();
    }
    out
}

fn text(prompt: &str, arguments: Value, served: &Toolsets) -> String {
    let got = get(&json!({"name": prompt, "arguments": arguments}), served).unwrap_or_else(|e| panic!("{prompt}: {}", e.message));
    assert_eq!(got["messages"].as_array().map(Vec::len), Some(1));
    assert_eq!(got["messages"][0]["role"], "user");
    assert_eq!(got["messages"][0]["content"]["type"], "text");
    got["messages"][0]["content"]["text"].as_str().expect("text").to_owned()
}

/// What `text` writes in backticks: tools, and nothing else.
fn quoted(text: &str) -> BTreeSet<String> {
    text.split('`').skip(1).step_by(2).map(str::to_owned).collect()
}

#[test]
fn every_tool_a_prompt_names_is_a_tool_and_on_its_list() {
    for prompt in PROMPTS {
        for tool in prompt.tools {
            assert!(tools::exists(tool), "`{}` names `{tool}`, which isn't a tool (renamed?)", prompt.name);
            assert_ne!(toolset(tool), "all", "`{tool}` is in no toolset");
        }
        let mut seen = BTreeSet::new();
        for arguments in variants(prompt) {
            let body = (prompt.render)(&check(prompt, &json!({"arguments": arguments})).expect("valid"));
            for word in quoted(&body) {
                assert!(prompt.tools.contains(&word.as_str()), "`{}` ({arguments}) names `{word}`, not on its list", prompt.name);
                seen.insert(word);
            }
        }
        let unused: Vec<_> = prompt.tools.iter().filter(|t| !seen.contains(**t)).collect();
        assert!(unused.is_empty(), "`{}` lists {unused:?} but never names them", prompt.name);
    }
}

#[test]
fn every_tool_is_served_or_named_with_its_toolset() {
    for served in [Toolsets::default(), Toolsets::all(), Toolsets::parse("debug").expect("debug"), Toolsets::parse("host").expect("host")] {
        for prompt in PROMPTS {
            for arguments in variants(prompt) {
                let text = text(prompt.name, arguments.clone(), &served);
                let (body, note) = text.rsplit_once("\n\nToolsets: ").expect("a toolsets note");
                let tools = named(prompt, body);
                assert!(!tools.is_empty(), "{}", prompt.name);
                for tool in &tools {
                    let set = toolset(tool);
                    assert!(note.contains(&format!("{set} has ")), "{}: {tool} not under {set}: {note}", prompt.name);
                    if served.contains(tool) {
                        continue;
                    }
                    let (_, not) = note.split_once(" and not ").unwrap_or_else(|| panic!("{}: `{tool}` isn't served: {note}", prompt.name));
                    assert!(not.contains(&format!("`{tool}`")), "{}: {note}", prompt.name);
                    let want = not.split(&format!("{TOOLSETS_ENV}=")).nth(1).and_then(|w| w.split(' ').next()).expect("a setting");
                    let parsed = Toolsets::parse(want).expect("the setting parses");
                    assert!(parsed.contains(tool), "{}: {want} doesn't serve `{tool}`", prompt.name);
                    assert!(tools.iter().all(|t| parsed.contains(t)), "{}: {want} serves them all", prompt.name);
                }
                if tools.iter().all(|t| served.contains(t)) {
                    assert!(note.ends_with("This server serves all of them."), "{note}");
                }
            }
        }
    }
    let core = text("boot-and-install", json!({"platform": "ios", "form_factor": "phone", "app_path": "/a.app"}), &Toolsets::default());
    assert!(core.contains("SIMEE_MCP_TOOLSETS=core,lifecycle (or all)"), "{core}");
    assert!(core.contains("core has `devices`, `boot`, `install`, `apps`, `launch`, `screenshot`"), "{core}");
}

#[test]
fn arguments_go_into_the_text() {
    let all = Toolsets::all();
    let t = text("boot-and-install", json!({"platform": "android", "form_factor": "tablet", "app_path": "/x/app.apk"}), &all);
    assert!(t.contains("`boot` {device, admission: \"wait\"}") && t.contains("path: \"/x/app.apk\"") && t.contains("android tablet"));
    let t = text("reproduce-bug", json!({"steps": "1. open settings", "capture": "logs"}), &all);
    assert!(t.contains("1. open settings") && t.contains("`logs` {device, stream: true") && t.contains("`devices` {booted: true}"));
    assert!(!t.contains("screenshots in order"), "{t}");
    let t = text("reproduce-bug", json!({"device": "Pixel", "steps": "s", "capture": "recording"}), &all);
    assert!(t.contains("on \"Pixel\"") && t.contains("screenshots in order") && !t.contains("`logs`") && !t.contains("`devices`"), "{t}");
    let t = text("across-form-factors", json!({"app_path": "/a.app", "form_factors": " phone , ,watch"}), &all);
    assert!(t.contains("each of: phone, watch.") && t.contains("admission: \"wait\""), "{t}");
    let t = text("audit-contrast", json!({"screen_description": "Login"}), &all);
    assert!(t.contains("\"Login\"") && t.contains("appearance: \"both\""), "{t}");
}

#[test]
fn bad_arguments_are_invalid_params() {
    let all = Toolsets::all();
    let refused = |params: Value| get(&params, &all).expect_err("refused");
    let e = refused(json!({"name": "boot-and-install", "arguments": {"platform": "ios"}}));
    assert_eq!((e.code, e.data), (INVALID_PARAMS, Some(json!({"missing": ["form_factor", "app_path"]}))));
    assert_eq!(e.message, "`boot-and-install` needs `form_factor`, `app_path`");
    // Blank is missing; no arguments at all is every required one missing.
    let e = refused(json!({"name": "find-slowness", "arguments": {"device": "  "}}));
    assert_eq!(e.data, Some(json!({"missing": ["device"]})));
    assert_eq!(refused(json!({"name": "reproduce-bug"})).data, Some(json!({"missing": ["steps", "capture"]})));
    for (params, says) in [
        (json!({}), "`name` is required"),
        (json!({"name": "nope"}), "Unknown prompt: nope (there are boot-and-install"),
        (json!({"name": "find-slowness", "arguments": ["d"]}), "`arguments` must be an object"),
        (json!({"name": "find-slowness", "arguments": {"device": 3}}), "`device` must be a string"),
        (json!({"name": "find-slowness", "arguments": {"device": "d", "pid": "1"}}), "takes no argument `pid` (it takes device)"),
        (json!({"name": "reproduce-bug", "arguments": {"steps": "s", "capture": "video"}}), "`capture` is one of logs, recording, both"),
        (json!({"name": "across-form-factors", "arguments": {"app_path": "a", "form_factors": " , "}}), "at least one"),
    ] {
        let e = refused(params.clone());
        assert_eq!(e.code, INVALID_PARAMS, "{params}");
        assert!(e.message.contains(says), "{params}: {}", e.message);
    }
    // Optional ones may be left out or null.
    assert!(get(&json!({"name": "audit-contrast"}), &all).is_ok());
    assert!(get(&json!({"name": "audit-contrast", "arguments": null}), &all).is_ok());
}

#[test]
fn the_list_pages_and_titles_follow_the_revision() {
    let modern = ctx(MODERN);
    let whole = list(&json!({}), &modern).expect("list");
    let names: Vec<&str> = whole["prompts"].as_array().expect("prompts").iter().filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(names, PROMPTS.iter().map(|p| p.name).collect::<Vec<_>>());
    assert_eq!(whole.get("nextCursor"), None, "one page holds them all");
    assert_eq!(
        whole["prompts"][0]["arguments"][0],
        json!({"name": "platform", "title": "Platform", "description": "ios or android", "required": true})
    );
    // Pages of two, walked by their cursors, are the same list.
    let (mut walked, mut params) = (Vec::new(), json!({}));
    loop {
        let page = list_page(&params, &modern, 2).expect("page");
        let prompts = page["prompts"].as_array().expect("prompts");
        assert!(!prompts.is_empty() && prompts.len() <= 2);
        walked.extend(prompts.iter().cloned());
        match page.get("nextCursor") {
            Some(next) => params = json!({"cursor": next}),
            None => break,
        }
    }
    assert_eq!(Value::Array(walked), whole["prompts"]);
    for cursor in [json!("0"), json!(PROMPTS.len().to_string()), json!("x"), json!(2)] {
        assert_eq!(list(&json!({"cursor": cursor}), &modern).expect_err("invalid").code, INVALID_PARAMS, "{cursor}");
    }
    assert!(list(&json!({"cursor": null}), &modern).is_ok());
    for version in LEGACY {
        let page = list(&json!({}), &ctx(version)).expect("list");
        let has = page["prompts"][0].get("title").is_some();
        assert_eq!(has, *version != "2025-03-26", "{version}");
        assert_eq!(page["prompts"][0]["arguments"][0].get("title").is_some(), has, "{version}");
    }
}

#[test]
fn the_docs_name_every_prompt() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join("crates/sim-mcp/Cargo.toml").is_file() {
        eprintln!("skipped: not in the simee workspace, so docs/mcp.md isn't here");
        return;
    }
    let docs = std::fs::read_to_string(root.join("docs/mcp.md")).expect("docs/mcp.md");
    for prompt in PROMPTS {
        assert!(docs.contains(&format!("`{}`", prompt.name)), "docs/mcp.md doesn't name the prompt `{}`", prompt.name);
        for a in prompt.arguments {
            assert!(docs.contains(&format!("`{}", a.name)), "docs/mcp.md doesn't name `{}`'s argument `{}`", prompt.name, a.name);
        }
    }
}
