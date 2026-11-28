use serde_json::json;

use super::*;
use crate::backend::fake::Scratch;

fn entry(tool: &str, status: &'static str) -> Entry {
    Entry { client: "agent".into(), tool: tool.into(), args: json!({}), status, duration_ms: 3, detail: None, admission: None }
}

fn log_at(dir: &Scratch, max_bytes: u64) -> (AuditLog, PathBuf) {
    let path = dir.0.join("state").join("audit.log");
    (AuditLog::open(Config { path: path.clone(), max_bytes }).expect("open"), path)
}

#[test]
fn secrets_are_removed_by_key_wherever_they_are() {
    let args = json!({
        "device": "d",
        "password": "hunter2",
        "Authorization": "Bearer abc",
        "env": {"API_KEY": "k", "PATH": "/bin", "nested": {"client_secret": "s", "ok": 1}},
        "list": [{"token": "t"}, "fine"],
        "apiKey": null,
        "x-api-key": false,
    });
    let out = redact("exec", &args);
    assert_eq!(out["device"], "d");
    for path in ["/password", "/Authorization", "/env/API_KEY", "/env/nested/client_secret", "/list/0/token", "/apiKey", "/x-api-key"] {
        assert_eq!(out.pointer(path), Some(&json!(REDACTED)), "{path}");
    }
    assert_eq!(out["env"]["PATH"], "/bin");
    assert_eq!(out["env"]["nested"]["ok"], 1);
    assert_eq!(out["list"][1], "fine");
    assert!(!out.to_string().contains("hunter2"));
}

#[test]
fn secrets_are_masked_inside_text() {
    for (text, masked) in [
        ("password=hunter2", "password=[redacted]"),
        ("curl -d 'password: hunter2' x", "curl -d 'password: [redacted]' x"),
        ("a=1&token=abc123&b=2", "a=1&token=[redacted]&b=2"),
        ("TOKEN=abc", "TOKEN=[redacted]"),
        ("Authorization: Bearer abc.def-ghi", "Authorization: [redacted]"),
        ("curl -H 'Authorization: Bearer abc' x", "curl -H 'Authorization: [redacted]' x"),
        ("sent Bearer abc to x", "sent Bearer [redacted] to x"),
        ("https://me:pw@example.com/a/b", "https://[redacted]@example.com/a/b"),
        ("postgres://me:pw@db:5432/x and http://a:b@c", "postgres://[redacted]@db:5432/x and http://[redacted]@c"),
        ("--api_key=abc --other", "--api_key=[redacted] --other"),
        ("secret = \"quoted value\"", "secret = \"[redacted] value\""),
        // Not secrets.
        ("tokens: 5", "tokens: 5"),
        ("the token_count is 3", "the token_count is 3"),
        ("https://example.com/path?x=1", "https://example.com/path?x=1"),
        ("https://example.com/a@b", "https://example.com/a@b"),
        ("nothing here", "nothing here"),
        ("", ""),
        ("password", "password"),
        ("password=", "password="),
        ("bearer", "bearer"),
        ("héllo password=wörld ünïcode", "héllo password=[redacted] ünïcode"),
    ] {
        assert_eq!(scrub_string(text), masked, "{text}");
    }
}

#[test]
fn what_a_tool_carries_as_content_is_kept_by_size_not_by_text() {
    let out = redact("type", &json!({"device": "d", "text": "my password is hunter2"}));
    assert_eq!(out["text"], "[redacted] (22 bytes)");
    assert_eq!(redact("write", &json!({"device": "d", "path": "p", "base64": "QUJD"}))["base64"], "[redacted] (4 bytes)");
    assert_eq!(redact("defaults_write", &json!({"key": "k", "value": "v"}))["value"], "[redacted] (1 bytes)");
    assert_eq!(redact("prefs_batch", &json!({"ops": [{"key": "k", "value": "v"}]}))["ops"], REDACTED);
    assert_eq!(redact("push", &json!({"payload": {"aps": {"alert": "hi"}}}))["payload"], REDACTED);
    assert_eq!(redact("notify", &json!({"name": "a.b", "extras": {"k": "v"}}))["extras"], REDACTED);
    assert_eq!(redact("notify", &json!({"name": "a.b"}))["name"], "a.b");
    // The same word in another tool is an argument like any other.
    assert_eq!(redact("tap", &json!({"text": "kept"}))["text"], "kept");
    assert_eq!(redact("type", &json!({"device": "d"})).get("text"), None);
}

#[test]
fn long_text_is_cut_and_deep_nesting_is_dropped() {
    let long = "x".repeat(10_000);
    let out = redact("write", &json!({"path": long}));
    let kept = out["path"].as_str().expect("path");
    assert!(kept.len() < 300 && kept.contains("[10000 bytes]"), "{kept}");
    let multibyte = "é".repeat(1000);
    assert!(redact("tap", &json!({"s": multibyte}))["s"].as_str().expect("s").starts_with("éé"));
    let mut deep = json!("leaf-secret");
    for _ in 0..40 {
        deep = json!({"a": deep});
    }
    assert!(!redact("tap", &deep).to_string().contains("leaf-secret"));
}

#[cfg(unix)]
#[test]
fn the_log_is_private_and_so_is_its_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("audit-perms");
    let (log, path) = log_at(&dir, 1 << 20);
    log.record(&entry("tap", status::OK));
    let mode = |p: &Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().expect("dir")), 0o700);
    drop(log);

    // A file that exists with looser permissions is tightened when it is opened.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let _again = AuditLog::open(Config::new(path.clone())).expect("reopen");
    assert_eq!(mode(&path), 0o600);
}

#[cfg(unix)]
#[test]
fn the_log_is_never_written_through_a_link() {
    let dir = Scratch::new("audit-link");
    let target = dir.0.join("elsewhere");
    std::fs::write(&target, "mine\n").expect("target");
    let path = dir.0.join("audit.log");
    std::os::unix::fs::symlink(&target, &path).expect("link");
    assert!(AuditLog::open(Config::new(path)).is_err(), "opening a link is refused");
    assert_eq!(std::fs::read_to_string(&target).expect("target"), "mine\n");
}

#[test]
fn the_log_is_capped_and_rotated_and_each_file_is_private() {
    let dir = Scratch::new("audit-rotate");
    let (log, path) = log_at(&dir, 600);
    let total = 40;
    for n in 0..total {
        let mut e = entry("tap", status::OK);
        e.args = json!({"n": n});
        log.record(&e);
    }
    let named = |suffix: &str| PathBuf::from(format!("{}{suffix}", path.display()));
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    for file in [path.clone(), named(".1"), named(".2"), named(".3")] {
        assert!(file.is_file(), "{} exists", file.display());
        assert!(size(&file) <= 600, "{} is {} bytes, past the cap", file.display(), size(&file));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).expect("metadata").permissions().mode() & 0o777, 0o600, "{}", file.display());
        }
    }
    assert!(!named(".4").exists(), "at most {KEEP} rotated files");
    // What is kept is the most recent, in order, with nothing torn.
    let kept = query(&path, &Query { limit: Some(1000), ..Query::default() }).expect("query");
    let ns: Vec<u64> = kept.iter().map(|e| e["args"]["n"].as_u64().expect("n")).collect();
    assert!(ns.len() > 10 && ns.len() < total as usize, "{} of {total} kept", ns.len());
    assert_eq!(ns.last(), Some(&(total - 1)), "the newest is there");
    assert!(ns.windows(2).all(|w| w[1] == w[0] + 1), "unbroken, oldest first: {ns:?}");
}

#[test]
fn a_log_that_is_reopened_goes_on_where_it_stopped() {
    let dir = Scratch::new("audit-reopen");
    let (log, path) = log_at(&dir, 1 << 20);
    log.record(&entry("tap", status::OK));
    drop(log);
    let log = AuditLog::open(Config::new(path.clone())).expect("reopen");
    log.record(&entry("swipe", status::ERROR));
    let tools: Vec<String> =
        query(&path, &Query::default()).expect("query").iter().map(|e| e["tool"].as_str().expect("tool").to_owned()).collect();
    assert_eq!(tools, ["tap", "swipe"]);
    assert_eq!(log.path(), path);
}

#[test]
fn the_log_is_queried_by_tool_client_status_and_how_many() {
    let dir = Scratch::new("audit-query");
    let (log, path) = log_at(&dir, 1 << 20);
    for (client, tool, st) in [
        ("a", "tap", status::OK),
        ("b", "tap", status::REFUSED),
        ("a", "erase", status::REFUSED),
        ("b", "erase", status::OK),
        ("a", "tap", status::ERROR),
    ] {
        let mut e = entry(tool, st);
        e.client = client.into();
        log.record(&e);
    }
    let by = |q: Query| -> Vec<(String, String, String)> {
        query(&path, &q)
            .expect("query")
            .iter()
            .map(|e| {
                (
                    e["client"].as_str().unwrap_or("").to_owned(),
                    e["tool"].as_str().unwrap_or("").to_owned(),
                    e["status"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect()
    };
    assert_eq!(by(Query::default()).len(), 5);
    assert_eq!(by(Query { tool: Some("erase".into()), ..Query::default() }).len(), 2);
    assert_eq!(
        by(Query { client: Some("a".into()), status: Some("refused".into()), ..Query::default() }),
        [("a".into(), "erase".into(), "refused".into())]
    );
    let last_two = by(Query { limit: Some(2), ..Query::default() });
    assert_eq!(last_two.iter().map(|e| e.2.as_str()).collect::<Vec<_>>(), ["ok", "error"], "the most recent two, oldest first");
    assert!(by(Query { tool: Some("nope".into()), ..Query::default() }).is_empty());
    assert_eq!(by(Query { limit: Some(0), ..Query::default() }), []);
}

#[test]
fn a_torn_line_is_skipped_and_a_missing_log_is_an_error() {
    let dir = Scratch::new("audit-torn");
    let (log, path) = log_at(&dir, 1 << 20);
    log.record(&entry("tap", status::OK));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open")
        .write_all(b"{\"ts_ms\": 1, \"tool\"\nnot json\n[1]\n")
        .expect("write");
    log.record(&entry("swipe", status::OK));
    let tools: Vec<String> =
        query(&path, &Query::default()).expect("query").iter().map(|e| e["tool"].as_str().expect("tool").to_owned()).collect();
    assert_eq!(tools, ["tap", "swipe"]);
    let error = query(&dir.0.join("missing.log"), &Query::default()).expect_err("no log");
    assert!(error.to_string().contains("no audit log at"), "{error}");
}

#[test]
fn an_entry_is_one_line_a_person_can_read() {
    let dir = Scratch::new("audit-describe");
    let (log, path) = log_at(&dir, 1 << 20);
    let mut e = entry("erase", status::REFUSED);
    e.detail = Some("needs confirmation; password=hunter2".into());
    e.args = json!({"device": "d"});
    log.record(&e);
    let entries = query(&path, &Query::default()).expect("query");
    let line = describe(&entries[0]);
    assert!(
        line.contains("agent erase refused 3 ms: needs confirmation; password=[redacted]") && line.ends_with(r#"{"device":"d"}"#),
        "{line}"
    );
    assert!(!std::fs::read_to_string(&path).expect("log").contains("hunter2"), "a detail is scrubbed too");
}
