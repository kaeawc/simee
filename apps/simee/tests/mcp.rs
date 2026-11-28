//! `simee mcp` end to end, on any host: the real binary as a subprocess speaking MCP on stdio,
//! with its own SDK, AVD home and `$HOME`, and a fake simee (`sim_android::testing`) standing in
//! for a running emulator. Unix only until the API has a Windows transport: the fake's console and
//! display are Unix sockets.
#![cfg(unix)]

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
        Env { root }
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
        self.spawn(toolsets, None)
    }

    /// `simee mcp` (every tool) with `SIMEE_MCP_SCREEN_FPS` set to `fps`.
    fn mcp_fps(&self, fps: &str) -> Mcp {
        self.spawn(Some("all"), Some(fps))
    }

    fn spawn(&self, toolsets: Option<&str>, screen_fps: Option<&str>) -> Mcp {
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
            .env("HOME", self.root.join("home"))
            .env("ANDROID_HOME", self.root.join("sdk"))
            .env("ANDROID_AVD_HOME", self.root.join("avd"))
            .env_remove("ANDROID_SDK_ROOT")
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
