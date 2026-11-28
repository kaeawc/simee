//! `simee mcp` end to end, on any host: the real binary as a subprocess speaking MCP on stdio,
//! with its own SDK, AVD home and `$HOME`, and a fake simee (`sim_android::testing`) standing in
//! for a running emulator.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_android::testing::{fake_emulator, wait_for};

const VERSION: &str = "2026-07-28";
const IMAGE: &str = "system-images;android-36;google_apis;arm64-v8a";

struct Env {
    root: PathBuf,
    /// A stand-in for `emu` (`SIMEE_EMU`) in the servers this environment starts, if any.
    emu: Option<PathBuf>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Env {
    fn new(tag: &str) -> Env {
        // Under /tmp: the fake display socket's path must fit a unix socket address.
        let root = PathBuf::from("/tmp").join(format!("mcp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let d = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&d).expect("image dir");
        std::fs::write(d.join("system.img"), b"s").expect("system.img");
        std::fs::write(d.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        std::fs::create_dir_all(root.join("home")).expect("home");
        Env { root, emu: None }
    }

    fn android(&self) -> sim_android::Android {
        let mut a = sim_android::Android::new(emu_avd::Env { sdk_root: Some(self.root.join("sdk")), avd_home: self.root.join("avd") });
        a.running_dir = Some(self.root.join("home/Library/Caches/TemporaryItems/avd/running"));
        a
    }

    /// `simee mcp` serving every tool.
    fn mcp(&self) -> Mcp {
        self.mcp_with(Some("all"))
    }

    /// `simee mcp` with `SIMEE_MCP_TOOLSETS` set to `toolsets` (unset: the default, `core`).
    fn mcp_with(&self, toolsets: Option<&str>) -> Mcp {
        self.spawn(toolsets, None, &["--admission", "ignore"])
    }

    /// `simee mcp` (every tool) with `SIMEE_MCP_SCREEN_FPS` set to `fps`.
    fn mcp_fps(&self, fps: &str) -> Mcp {
        self.spawn(Some("all"), Some(fps), &["--admission", "ignore"])
    }

    /// `simee mcp` (every tool) with these flags and no others: the host's load decides whether a
    /// boot or a creation is admitted, as it does for any agent.
    fn mcp_flags(&self, flags: &[&str]) -> Mcp {
        self.spawn(Some("all"), None, flags)
    }

    /// Where `simee mcp` keeps its audit log (and its token) in this environment.
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// `simee` with these arguments, in this environment.
    fn simee(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_simee"))
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("SIMEE_STATE_DIR", self.state())
            .output()
            .expect("simee runs")
    }

    fn spawn(&self, toolsets: Option<&str>, screen_fps: Option<&str>, flags: &[&str]) -> Mcp {
        let mut command = Command::new(env!("CARGO_BIN_EXE_simee"));
        match toolsets {
            Some(t) => command.env("SIMEE_MCP_TOOLSETS", t),
            None => command.env_remove("SIMEE_MCP_TOOLSETS"),
        };
        match screen_fps {
            Some(f) => command.env("SIMEE_MCP_SCREEN_FPS", f),
            None => command.env_remove("SIMEE_MCP_SCREEN_FPS"),
        };
        let mut child = command
            .arg("mcp")
            .args(flags)
            .env("HOME", self.root.join("home"))
            .env("SIMEE_STATE_DIR", self.state())
            .env("ANDROID_HOME", self.root.join("sdk"))
            .env("ANDROID_AVD_HOME", self.root.join("avd"))
            .env_remove("ANDROID_SDK_ROOT")
            .envs(self.emu.iter().map(|emu| ("SIMEE_EMU", emu)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("simee mcp runs");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Mcp { child, stdin, stdout, next: 1 }
    }
}

/// A client on the subprocess's pipes.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write");
        stdin.flush().expect("flush");
    }

    fn line(&mut self) -> Value {
        let mut line = String::new();
        assert!(self.stdout.read_line(&mut line).expect("read") > 0, "the server closed stdout");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout carries only JSON-RPC, got {line:?}: {e}"))
    }

    /// A modern request; its response (notifications in between are skipped).
    fn request(&mut self, method: &str, mut params: Value, tasks: bool) -> Value {
        let id = self.next;
        self.next += 1;
        let caps = if tasks { json!({"extensions": {"io.modelcontextprotocol/tasks": {}}}) } else { json!({}) };
        params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": VERSION, "io.modelcontextprotocol/clientCapabilities": caps});
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let message = self.line();
            if message["id"] == id {
                return message;
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}), false)["result"].clone()
    }

    /// A task for `tool`, polled to its end.
    fn task(&mut self, tool: &str, arguments: Value) -> Value {
        let created = self.request("tools/call", json!({"name": tool, "arguments": arguments}), true);
        assert_eq!(created["result"]["resultType"], "task", "{created}");
        let id = created["result"]["taskId"].clone();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let task = self.request("tasks/get", json!({"taskId": id}), true)["result"].clone();
            if task["status"] != "working" {
                return task;
            }
            assert!(Instant::now() < deadline, "the task never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Closes stdin and waits for the server to exit, as a client does to shut it down.
    fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.child.wait().expect("exits")
    }
}

#[test]
fn an_agent_discovers_creates_and_drives_a_device() {
    let e = Env::new("drive");
    let mut mcp = e.mcp();

    let discover = mcp.request("server/discover", json!({}), false)["result"].clone();
    assert_eq!(discover["supportedVersions"][0], VERSION);
    assert_eq!(discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "simee");

    // Every API method is a tool.
    let tools = mcp.request("tools/list", json!({}), false)["result"]["tools"].clone();
    for name in ["devices", "create", "boot", "tap", "swipe", "screenshot", "console", "fold", "device_view"] {
        assert!(tools.as_array().is_some_and(|t| t.iter().any(|x| x["name"] == name)), "{name}");
    }

    // A device is created as a task (creating an AVD isn't instant on a real SDK) and then listed.
    let created = mcp.task("create", json!({"name": "fold", "platform": "android", "system_image": IMAGE, "form_factor": "foldable"}));
    assert_eq!(created["status"], "completed", "{created}");
    assert_eq!(created["result"]["structuredContent"]["form_factor"], "foldable");
    let devices = mcp.call("devices", json!({"platform": "android"}));
    assert_eq!(devices["structuredContent"][0]["udid"], "fold");

    // The device is a resource, and so is each thing about it.
    let resources = mcp.request("resources/list", json!({}), false)["result"]["resources"].clone();
    assert!(resources.as_array().is_some_and(|r| r.iter().any(|x| x["uri"] == "simee://devices/fold")));
    let info = mcp.request("resources/read", json!({"uri": "simee://devices/fold"}), false);
    assert!(info["result"]["contents"][0]["text"].as_str().is_some_and(|t| t.contains("fold")), "{info}");

    // A stopped device refuses input: a tool error, not a protocol one.
    let refused = mcp.call("tap", json!({"device": "fold", "x": 10, "y": 20}));
    assert_eq!(refused["isError"], true);
    assert!(refused["content"][0]["text"].as_str().is_some_and(|t| t.contains("isn't running")));

    // Once it runs (a fake emulator stands in for the boot), the same tools drive it.
    let fake = fake_emulator(&e.android(), "fold");
    let tap = mcp.call("tap", json!({"device": "fold", "x": 10, "y": 20}));
    assert_eq!(tap.get("isError"), None, "{tap}");
    mcp.call("swipe", json!({"device": "fold", "x1": 300, "y1": 900, "x2": 300, "y2": 300, "duration_ms": 50}));
    mcp.call("button", json!({"device": "fold", "button": "home"}));
    mcp.call("fold", json!({"device": "fold", "posture": "half-opened"}));
    let display = wait_for(&fake.display, 4);
    assert_eq!(display[..2], ["touch 10 20 down", "touch 10 20 up"]);
    let console = wait_for(&fake.console, 2);
    assert!(console.iter().any(|l| l == "event send EV_KEY:KEY_HOME:1"), "{console:?}");

    // Its screen is an image to the agent and a resource to the client.
    let shot = mcp.call("screenshot", json!({"device": "fold"}));
    assert_eq!(shot["content"][0]["type"], "image");
    assert!(shot["content"][0]["data"].as_str().is_some_and(|d| d.starts_with("iVBOR")), "a PNG");
    let resource = mcp.request("resources/read", json!({"uri": "simee://devices/fold/screenshot"}), false);
    assert_eq!(resource["result"]["contents"][0]["mimeType"], "image/png");
    let view = mcp.call("device_view", json!({"device": "fold"}));
    assert!(view["structuredContent"]["screen"]["width"].is_number(), "{view}");

    // Closing stdin is the client's goodbye: the server exits cleanly.
    assert!(mcp.close().success());
}

#[test]
fn a_client_of_an_older_revision_opens_with_initialize() {
    let e = Env::new("legacy");
    let mut mcp = e.mcp();
    mcp.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}));
    let init = mcp.line();
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    mcp.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "form_factors", "arguments": {"platform": "android"}}}));
    let result = mcp.line();
    assert_eq!(result["id"], 2);
    assert!(result["result"]["content"][0]["text"].as_str().is_some_and(|t| t.contains("Wear OS")), "{result}");
    assert!(mcp.close().success());
}

#[test]
fn the_default_server_serves_core_and_says_where_the_rest_is() {
    let e = Env::new("core");
    let mut mcp = e.mcp_with(None);
    let tools = mcp.request("tools/list", json!({}), false)["result"]["tools"].clone();
    let names: Vec<&str> = tools.as_array().expect("tools").iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.len() <= 25 && names.contains(&"tap") && names.contains(&"device_view") && !names.contains(&"create"), "{names:?}");
    let hidden = mcp.request("tools/call", json!({"name": "create", "arguments": {"name": "x"}}), false);
    assert!(hidden["error"]["message"].as_str().is_some_and(|m| m.contains("SIMEE_MCP_TOOLSETS=core,lifecycle")), "{hidden}");
    assert!(mcp.close().success());
    // An unknown toolset stops the server before it serves anything.
    assert!(!e.mcp_with(Some("core,nope")).close().success());
}

#[test]
fn the_screen_subscription_rate_is_an_environment_setting_that_must_make_sense() {
    let e = Env::new("fps");
    let mut mcp = e.mcp_fps("5");
    let templates = mcp.request("resources/templates/list", json!({}), false)["result"]["resourceTemplates"].clone();
    let names: Vec<&str> = templates.as_array().expect("templates").iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"screen") && names.contains(&"processes") && names.contains(&"prefs"), "{names:?}");
    let discover = mcp.request("server/discover", json!({}), false);
    assert_eq!(discover["result"]["capabilities"]["resources"], json!({"subscribe": true, "listChanged": true}));
    assert!(mcp.close().success());
    // Zero, too many and not a number stop the server before it serves anything.
    for bad in ["0", "31", "fast"] {
        assert!(!e.mcp_fps(bad).close().success(), "{bad}");
    }
}

#[test]
fn the_default_server_lists_prompts_and_says_which_toolsets_they_need() {
    let e = Env::new("prompts");
    let mut mcp = e.mcp_with(None);
    let list = mcp.request("prompts/list", json!({}), false)["result"].clone();
    let names: Vec<&str> = list["prompts"].as_array().expect("prompts").iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(names.contains(&"boot-and-install") && names.contains(&"find-slowness"), "{names:?}");
    let arguments = json!({"platform": "android", "form_factor": "phone", "app_path": "/a.apk"});
    let got = mcp.request("prompts/get", json!({"name": "boot-and-install", "arguments": arguments}), false);
    let text = got["result"]["messages"][0]["content"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("`install` {device, path: \"/a.apk\"}") && text.contains("SIMEE_MCP_TOOLSETS=core,lifecycle"), "{got}");
    let missing = mcp.request("prompts/get", json!({"name": "boot-and-install", "arguments": {"platform": "ios"}}), false);
    assert_eq!(missing["error"]["code"], -32602, "{missing}");
    assert!(mcp.close().success());
}

/// The text of a tool result.
fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

fn create_fold(mcp: &mut Mcp) -> Value {
    mcp.task("create", json!({"name": "fold", "platform": "android", "system_image": IMAGE, "form_factor": "foldable"}))
}

#[test]
fn deleting_a_device_needs_the_users_confirmation_and_every_call_is_in_the_audit_log() {
    let e = Env::new("confirm");
    let mut mcp = e.mcp_flags(&["--admission", "ignore"]);
    assert_eq!(create_fold(&mut mcp)["status"], "completed");

    // The agent can't do it on its own say-so.
    let refused = mcp.call("delete", json!({"device": "fold"}));
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(text(&refused).contains("needs the user's confirmation") && text(&refused).contains("`confirm: true`"), "{refused}");
    assert_eq!(mcp.call("devices", json!({"platform": "android"}))["structuredContent"][0]["udid"], "fold", "the device is still there");
    // With the user's agreement it goes through.
    assert_eq!(mcp.call("delete", json!({"device": "fold", "confirm": true})).get("isError"), None);
    assert_eq!(mcp.call("devices", json!({"platform": "android"}))["structuredContent"], json!([]));
    assert!(mcp.close().success());

    // The log has each call, and `simee mcp-audit` reads it.
    let log = e.state().join("mcp-audit.log");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&log).expect("log").permissions().mode() & 0o777, 0o600);
    }
    let shown = e.simee(&["mcp-audit", "--json"]);
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    let entries: Vec<Value> =
        String::from_utf8(shown.stdout).expect("utf-8").lines().map(|l| serde_json::from_str(l).expect("JSON line")).collect();
    let seen: Vec<(&str, &str)> = entries.iter().map(|e| (e["tool"].as_str().unwrap_or(""), e["status"].as_str().unwrap_or(""))).collect();
    assert_eq!(
        seen,
        [("create", "ok"), ("delete", "refused"), ("devices", "ok"), ("delete", "ok"), ("devices", "ok")],
        "one entry per call, in order"
    );
    let refused_only = e.simee(&["mcp-audit", "--status", "refused", "--tool", "delete"]);
    let text = String::from_utf8(refused_only.stdout).expect("utf-8");
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("delete refused") && text.contains("needs the user's confirmation"), "{text}");
    assert!(!e.simee(&["mcp-audit", "--log", "/nonexistent/audit.log"]).status.success());
}

#[test]
fn a_read_only_server_lists_and_runs_only_what_changes_nothing() {
    let e = Env::new("readonly");
    let mut mcp = e.mcp_flags(&["--read-only", "--no-audit-log"]);
    let tools = mcp.request("tools/list", json!({}), false)["result"]["tools"].clone();
    let tools = tools.as_array().expect("tools");
    assert!(tools.iter().any(|t| t["name"] == "devices") && !tools.iter().any(|t| t["name"] == "tap" || t["name"] == "create"));
    let refused = mcp.call("create", json!({"name": "fold", "platform": "android", "system_image": IMAGE}));
    assert_eq!(refused["isError"], true);
    assert!(text(&refused).contains("read-only"), "{refused}");
    assert_eq!(mcp.call("devices", json!({"platform": "android"}))["structuredContent"], json!([]));
    assert!(mcp.close().success());
    assert!(!e.state().join("mcp-audit.log").exists(), "--no-audit-log keeps none");
}

#[test]
fn the_device_flag_limits_which_devices_a_client_may_make_and_touch() {
    let e = Env::new("allow");
    let mut mcp = e.mcp_flags(&["--allow-device", "ci-*", "--admission", "ignore"]);
    let refused = mcp.task("create", json!({"name": "fold", "platform": "android", "system_image": IMAGE}));
    assert!(refused["result"]["content"][0]["text"].as_str().is_some_and(|t| t.contains("couldn't touch afterwards")), "{refused}");
    let made = mcp.task("create", json!({"name": "ci-fold", "platform": "android", "system_image": IMAGE}));
    assert_eq!(made["status"], "completed", "{made}");
    assert_eq!(mcp.call("devices", json!({"platform": "android"}))["structuredContent"][0]["udid"], "ci-fold");
    assert!(mcp.close().success());
}

#[test]
fn a_policy_file_gives_each_client_its_own_rules() {
    let e = Env::new("policy");
    let file = e.root.join("policy.json");
    std::fs::write(&file, r#"{"clients": {"reviewer": {"read_only": true}}}"#).expect("policy");
    let mut mcp = e.mcp_flags(&["--policy", file.to_str().expect("utf-8"), "--no-audit-log", "--admission", "ignore"]);
    let as_client = |mcp: &mut Mcp, name: &str, tool: &str, args: Value| {
        let params = json!({"name": tool, "arguments": args, "_meta": {"io.modelcontextprotocol/protocolVersion": VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}, "io.modelcontextprotocol/clientInfo": {"name": name}}});
        let id = mcp.next;
        mcp.next += 1;
        mcp.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
        loop {
            let message = mcp.line();
            if message["id"] == id {
                return message["result"].clone();
            }
        }
    };
    let make = json!({"name": "fold", "platform": "android", "system_image": IMAGE});
    let refused = as_client(&mut mcp, "reviewer", "create", make.clone());
    assert!(refused["isError"] == true && text(&refused).contains("read-only"), "{refused}");
    // Another client is not held to it. (`create` runs inline for a client without the tasks extension.)
    let made = as_client(&mut mcp, "builder", "create", make);
    assert_eq!(made.get("isError"), None, "{made}");
    assert!(mcp.close().success());
    // A policy that doesn't parse stops the server before it serves anything.
    std::fs::write(&file, r#"{"clients": {"reviewer": {"readonly": true}}}"#).expect("policy");
    assert!(!e.mcp_flags(&["--policy", file.to_str().expect("utf-8")]).close().success());
}

/// An `emu` that starts and never finishes booting: a `boot` that runs it is held until cancelled.
/// It records its pid, so a test that fails before the shutdown has stopped it can stop it.
struct HeldEmu {
    pid_file: PathBuf,
}

impl HeldEmu {
    fn install(e: &mut Env) -> HeldEmu {
        use std::os::unix::fs::PermissionsExt as _;
        let (script, pid_file) = (e.root.join("held-emu.sh"), e.root.join("held-emu.pid"));
        std::fs::write(&script, format!("#!/bin/sh\necho $$ > {}\nexec sleep 600\n", pid_file.display())).expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        e.emu = Some(script);
        HeldEmu { pid_file }
    }

    /// Waits until the emulator has started: the boot is in flight.
    fn started(&self) {
        let until = Instant::now() + DEADLINE;
        while std::fs::read_to_string(&self.pid_file).map_or(true, |p| p.trim().is_empty()) {
            assert!(Instant::now() < until, "the boot never started the emulator");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for HeldEmu {
    fn drop(&mut self) {
        // The pid this script wrote: not a process found by name.
        if let Ok(pid) = std::fs::read_to_string(&self.pid_file) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

/// A deadline for a hang, never a measure of speed.
const DEADLINE: Duration = Duration::from_secs(60);

fn signal(child: &Child, sig: &str) {
    let ok = Command::new("kill").args([sig, &child.id().to_string()]).status().expect("kill").success();
    assert!(ok, "kill {sig}");
}

fn exit_code(child: &mut Child) -> Option<i32> {
    let until = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status.code();
        }
        assert!(Instant::now() < until, "simee mcp didn't exit within {DEADLINE:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sigterm_answers_a_held_call_that_the_shutdown_cancelled_and_exits_143() {
    let mut e = Env::new("term");
    let held = HeldEmu::install(&mut e);
    let mut mcp = e.mcp();
    assert_eq!(create_fold(&mut mcp)["status"], "completed");
    // A boot that waits on an emulator that never comes up.
    let id = mcp.next;
    mcp.next += 1;
    let params = json!({"name": "boot", "arguments": {"device": "fold"},
        "_meta": {"io.modelcontextprotocol/protocolVersion": VERSION, "io.modelcontextprotocol/clientCapabilities": {}}});
    mcp.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
    held.started();

    signal(&mcp.child, "-TERM");
    // The held call is answered, not dropped with the process: cancelled by the shutdown.
    let answer = loop {
        let message = mcp.line();
        if message["id"] == id {
            break message;
        }
    };
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert!(text(&answer["result"]).contains("cancelled: server shutting down"), "{answer}");
    assert_eq!(exit_code(&mut mcp.child), Some(143));
}

/// `simee mcp --http` on a free loopback port, without authentication, and the address it says it
/// serves on.
fn spawn_http(e: &Env) -> (Child, std::net::SocketAddr) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_simee"))
        .args(["mcp", "--http", "127.0.0.1:0", "--no-auth", "--admission", "ignore", "--no-audit-log"])
        .env("SIMEE_MCP_TOOLSETS", "all")
        .env("HOME", e.root.join("home"))
        .env("SIMEE_STATE_DIR", e.state())
        .env("ANDROID_HOME", e.root.join("sdk"))
        .env("ANDROID_AVD_HOME", e.root.join("avd"))
        .env_remove("ANDROID_SDK_ROOT")
        .envs(e.emu.iter().map(|emu| ("SIMEE_EMU", emu)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("simee mcp --http runs");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    let addr = loop {
        line.clear();
        assert!(stderr.read_line(&mut line).expect("read") > 0, "simee mcp --http exited before it said where it serves");
        if let Some(rest) = line.split("Streamable HTTP on http://").nth(1) {
            break rest.split('/').next().expect("an address").parse().expect("a socket address");
        }
    };
    // What it prints later (the shutdown's lines) must not fill a pipe nobody reads.
    std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()).map(drop).unwrap_or_default());
    (child, addr)
}

/// One `tools/call` over HTTP (the current revision, so no session): the response, parsed.
fn http_call(addr: std::net::SocketAddr, id: u64, tool: &str, arguments: Value) -> Value {
    use std::io::Read as _;
    let body = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": arguments,
        "_meta": {"io.modelcontextprotocol/protocolVersion": VERSION, "io.modelcontextprotocol/clientCapabilities": {}}}})
    .to_string();
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(DEADLINE)).expect("timeout");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         MCP-Protocol-Version: {VERSION}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).expect("the server answers");
    let reply = String::from_utf8_lossy(&reply).into_owned();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or_else(|| panic!("no HTTP reply: {reply:?}"));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    serde_json::from_str(body.trim()).unwrap_or_else(|e| panic!("a JSON body, got {body:?}: {e}"))
}

#[test]
fn sigterm_on_the_http_transport_answers_a_held_call_that_the_shutdown_cancelled_and_exits_143() {
    let mut e = Env::new("http-term");
    let held = HeldEmu::install(&mut e);
    let (mut child, addr) = spawn_http(&e);
    let made = http_call(addr, 1, "create", json!({"name": "fold", "platform": "android", "system_image": IMAGE}));
    assert_eq!(made["result"].get("isError"), None, "{made}");

    let boot = std::thread::spawn(move || http_call(addr, 2, "boot", json!({"device": "fold"})));
    held.started();
    signal(&child, "-TERM");
    let answer = boot.join().expect("the held call is answered, not dropped with the process");
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert!(text(&answer["result"]).contains("cancelled: server shutting down"), "{answer}");
    assert_eq!(exit_code(&mut child), Some(143));
}
