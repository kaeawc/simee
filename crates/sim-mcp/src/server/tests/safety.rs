//! Confirmations, the device and toolset policy, read-only servers, admission and the audit log,
//! through the protocol. Nothing here waits on a clock: a call is answered when the rig has what it
//! needs, and an unanswered question is a zero-length wait.

use super::*;
use crate::audit::{self, AuditLog};
use crate::backend::fake::Scratch;
use crate::policy::Policy;
use crate::rpc::META_CLIENT_INFO;

const CI_1: &str = "UDID-CI-1";
const CI_2: &str = "UDID-CI-2";
const PROD: &str = "UDID-PROD";

/// Devices that answer `info` and `devices` as the API does (the fake answers a method the same way
/// whatever it is asked), and change as boots, creations and deletions happen; everything else is
/// the fake's, so its calls are recorded.
struct World {
    fake: Arc<Fake>,
    devices: Mutex<Vec<Value>>,
}

fn device(udid: &str, name: &str, state: &str) -> Value {
    json!({"udid": udid, "name": name, "state": state, "platform": "ios", "form_factor": "phone"})
}

impl World {
    fn new(fake: Arc<Fake>) -> Arc<World> {
        let devices = vec![device(CI_1, "ci-1", "Shutdown"), device(CI_2, "ci-2", "Booted"), device(PROD, "prod-1", "Booted")];
        Arc::new(World { fake, devices: Mutex::new(devices) })
    }

    fn set_state(&self, udid: &str, state: &str) {
        for d in self.devices.lock().expect("devices").iter_mut().filter(|d| d["udid"] == udid) {
            d["state"] = state.into();
        }
    }
}

impl Backend for World {
    fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
        let query = params["device"].as_str().unwrap_or("booted").to_owned();
        let found = |devices: &[Value]| {
            devices
                .iter()
                .find(
                    |d| if query == "booted" { d["state"] == "Booted" } else { d["udid"] == query.as_str() || d["name"] == query.as_str() },
                )
                .cloned()
        };
        match method {
            "info" => {
                let devices = self.devices.lock().expect("devices");
                found(&devices).map(|d| json!({"device": d})).ok_or_else(|| format!("no device matches `{query}`"))
            }
            "devices" => Ok(Value::Array(self.devices.lock().expect("devices").clone())),
            "create" | "clone" => {
                let name = params["name"].as_str().unwrap_or("new");
                let made = device(&format!("UDID-{}", name.to_uppercase()), name, "Shutdown");
                self.devices.lock().expect("devices").push(made.clone());
                self.fake.call(method, params)?;
                Ok(made)
            }
            "boot" => {
                let udid = found(&self.devices.lock().expect("devices")).map(|d| d["udid"].as_str().unwrap_or("").to_owned());
                self.set_state(&udid.unwrap_or_default(), "Booted");
                self.fake.call(method, params)
            }
            "shutdown" | "erase" | "delete" => {
                let result = self.fake.call(method, params);
                if method == "shutdown" {
                    let udid = found(&self.devices.lock().expect("devices")).map(|d| d["udid"].as_str().unwrap_or("").to_owned());
                    self.set_state(&udid.unwrap_or_default(), "Shutdown");
                }
                if method == "delete" {
                    let mut devices = self.devices.lock().expect("devices");
                    devices.retain(|d| d["udid"] != query.as_str() && d["name"] != query.as_str());
                }
                result
            }
            _ => self.fake.call(method, params),
        }
    }
}

fn rig(policy: Policy) -> Rig {
    rig_logging(policy, None)
}

fn rig_logging(policy: Policy, audit: Option<AuditLog>) -> Rig {
    rig_world(policy, audit).0
}

/// A rig and its devices.
fn rig_world(policy: Policy, audit: Option<AuditLog>) -> (Rig, Arc<World>) {
    let safety = Arc::new(Safety::new(policy, audit));
    let world = Mutex::new(None);
    let rig = Rig::guarded(
        Toolsets::all(),
        |fake| {
            let devices = World::new(fake);
            *world.lock().expect("world") = Some(devices.clone());
            devices
        },
        safety,
    );
    let devices = world.lock().expect("world").take().expect("the rig made its devices");
    (rig, devices)
}

/// A modern client of this name with these capabilities.
fn meta(client: &str, caps: Value) -> Value {
    json!({META_VERSION: MODERN, META_CLIENT_CAPS: caps, META_CLIENT_INFO: {"name": client, "version": "1"}})
}

fn call_as(rig: &Rig, client: &str, caps: Value, tool: &str, args: Value) -> Value {
    let id = rig.send_raw("tools/call", json!({"name": tool, "arguments": args, "_meta": meta(client, caps)}));
    rig.response(id)["result"].clone()
}

/// A client that can't be asked.
fn call(rig: &Rig, tool: &str, args: Value) -> Value {
    call_as(rig, "agent", json!({}), tool, args)
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or("")
}

fn is_error(result: &Value) -> bool {
    result["isError"] == true
}

fn devices_only(patterns: &[&str]) -> Policy {
    let patterns: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
    Policy::default().with_default(|p| p.devices = Some(patterns))
}

// ---- confirmation --------------------------------------------------------------------------

#[test]
fn a_destructive_call_without_confirmation_is_refused_and_says_how_to_confirm() {
    let rig = rig(Policy::default());
    for (tool, args) in [
        ("delete", json!({"device": "ci-1"})),
        ("erase", json!({"device": "ci-1"})),
        ("kill", json!({"device": "ci-2", "pid": 123})),
        ("clear_container", json!({"device": "ci-2", "path": "Data/Application/x"})),
        ("reset_keychain", json!({"device": "ci-2"})),
        // Data nothing replaces.
        ("rm", json!({"device": "ci-2", "path": "Data/x"})),
        ("uninstall", json!({"device": "ci-2", "bundle_id": "com.example"})),
        ("prefs_delete", json!({"device": "ci-2", "bundle_id": "com.example", "store": "visible", "key": "k"})),
        (
            "prefs_batch",
            json!({"device": "ci-2", "bundle_id": "com.example", "store": "visible", "ops": [{"key": "a", "type": "string", "value": "1"}, {"key": "b", "delete": true}]}),
        ),
        // The console's own ways to stop an emulator and delete a snapshot.
        ("console", json!({"device": "ci-2", "command": "kill"})),
        ("console", json!({"device": "ci-2", "commands": ["help", "avd stop"]})),
        ("console", json!({"device": "ci-2", "command": "AVD  Snapshot delete fresh"})),
        // A device this client didn't boot.
        ("shutdown", json!({"device": "ci-2"})),
    ] {
        let result = call(&rig, tool, args);
        assert!(is_error(&result), "{tool}: {result}");
        let message = text(&result);
        assert!(message.starts_with("refused: "), "{tool}: {message}");
        assert!(message.contains(&format!("`{tool}` needs the user's confirmation")), "{message}");
        assert!(message.contains("`confirm: true`") && message.contains("nothing was done"), "{message}");
        assert!(message.contains("doesn't support elicitation"), "says why it wasn't asked: {message}");
        assert!(rig.fake.called(tool).is_empty(), "{tool} never reached the device");
    }
    // `confirm` has to be true, not just present.
    assert!(is_error(&call(&rig, "erase", json!({"device": "ci-1", "confirm": false}))));
    assert!(is_error(&call(&rig, "erase", json!({"device": "ci-1", "confirm": "true"}))));
    assert!(rig.fake.called("erase").is_empty());
}

#[test]
fn confirm_true_lets_a_client_that_cant_be_asked_through_and_the_device_never_sees_it() {
    let rig = rig(Policy::default());
    let result = call(&rig, "erase", json!({"device": "ci-1", "confirm": true}));
    assert!(!is_error(&result), "{result}");
    assert_eq!(rig.fake.called("erase"), [json!({"device": "ci-1"})], "`confirm` is the server's, not the method's");
    // What isn't destructive is never asked about.
    assert!(!is_error(&call(&rig, "tap", json!({"device": "ci-2", "x": 1, "y": 1}))));
}

#[test]
fn a_policy_can_make_another_tool_need_confirmation() {
    let policy = Policy::parse(r#"{"default": {"confirm": ["uninstall"]}}"#).expect("policy");
    let rig = rig(policy);
    let refused = call(&rig, "uninstall", json!({"device": "ci-2", "bundle_id": "com.example"}));
    assert!(is_error(&refused) && text(&refused).contains("`uninstall` needs the user's confirmation"), "{refused}");
    assert!(!is_error(&call(&rig, "uninstall", json!({"device": "ci-2", "bundle_id": "com.example", "confirm": true}))));
}

fn asks() -> Value {
    json!({"elicitation": {}})
}

fn input_required(result: &Value) -> (Value, String) {
    assert_eq!(result["resultType"], "input_required", "{result}");
    let ask = &result["inputRequests"]["confirm"];
    assert_eq!(ask["method"], "elicitation/create");
    assert_eq!(ask["params"]["mode"], "form");
    assert_eq!(ask["params"]["requestedSchema"]["properties"]["confirm"]["type"], "boolean");
    assert_eq!(ask["params"]["requestedSchema"]["properties"]["confirm"]["default"], false, "the form starts at no");
    (ask["params"].clone(), result["requestState"].as_str().expect("requestState").to_owned())
}

fn retry(rig: &Rig, args: Value, state: &str, answer: Value) -> Value {
    let params = json!({"name": "erase", "arguments": args, "requestState": state, "inputResponses": {"confirm": answer},
        "_meta": meta("agent", asks())});
    let id = rig.send_raw("tools/call", params);
    rig.response(id)["result"].clone()
}

#[test]
fn a_client_that_can_be_asked_is_asked_in_the_result_and_its_confirm_argument_counts_for_nothing() {
    let rig = rig(Policy::default());
    // `confirm: true` from the agent doesn't replace the person.
    let first = call_as(&rig, "agent", asks(), "erase", json!({"device": "ci-1", "confirm": true}));
    let (form, state) = input_required(&first);
    let message = form["message"].as_str().expect("message");
    assert!(message.contains("erase `ci-1` (UDID-CI-1)"), "the person reads what is being erased: {message}");
    assert!(rig.fake.called("erase").is_empty());

    // The person says yes: the retry with the answer and the state runs the call.
    let args = json!({"device": "ci-1", "confirm": true});
    let done = retry(&rig, args.clone(), &state, json!({"action": "accept", "content": {"confirm": true}}));
    assert!(!is_error(&done) && done["resultType"] == "complete", "{done}");
    assert_eq!(rig.fake.called("erase"), [json!({"device": "ci-1"})]);

    // No, dismissed, an accepted form that says no, a stranger's state and another call's state
    // each leave it undone.
    for (answer, state, args, reason) in [
        (json!({"action": "decline"}), state.clone(), args.clone(), "the user declined"),
        (json!({"action": "cancel"}), state.clone(), args.clone(), "dismissed"),
        (json!({"action": "accept", "content": {"confirm": false}}), state.clone(), args.clone(), "without allowing it"),
        (json!({"action": "accept"}), state.clone(), args.clone(), "without allowing it"),
    ] {
        let result = retry(&rig, args, &state, answer);
        assert!(is_error(&result) && text(&result).contains(reason), "{reason}: {result}");
    }
    for (state, args) in [("forged".to_owned(), args.clone()), (state.clone(), json!({"device": "ci-2"}))] {
        let again = retry(&rig, args, &state, json!({"action": "accept", "content": {"confirm": true}}));
        let (_, fresh) = input_required(&again);
        assert_ne!(fresh, state, "a yes to another call asks again");
    }
    assert_eq!(rig.fake.called("erase").len(), 1, "only the confirmed call ran");
}

#[test]
fn a_client_of_an_earlier_revision_is_asked_with_a_request_of_its_own() {
    let rig = rig(Policy::default());
    rig.server.ask_clients_to_confirm(WAIT);
    let init = rig.send_raw(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {"elicitation": {}}, "clientInfo": {"name": "old", "version": "1"}}),
    );
    rig.response(init);
    let erase = |rig: &Rig| rig.send_raw("tools/call", json!({"name": "erase", "arguments": {"device": "ci-1", "confirm": true}}));

    // Yes.
    let id = erase(&rig);
    let question = rig.notification("elicitation/create");
    assert!(question["id"].is_string(), "a request, which the client answers");
    assert!(question["params"]["message"].as_str().is_some_and(|m| m.contains("erase `ci-1`")), "{question}");
    rig.send(&json!({"jsonrpc": "2.0", "id": question["id"], "result": {"action": "accept", "content": {"confirm": true}}}));
    let done = rig.response(id);
    assert!(!is_error(&done["result"]), "{done}");
    assert_eq!(rig.fake.called("erase").len(), 1);

    // No.
    let id = erase(&rig);
    let question = rig.notification("elicitation/create");
    rig.send(&json!({"jsonrpc": "2.0", "id": question["id"], "result": {"action": "decline"}}));
    let refused = rig.response(id);
    assert!(is_error(&refused["result"]) && text(&refused["result"]).contains("the user declined"), "{refused}");
    assert_eq!(rig.fake.called("erase").len(), 1);

    // The client errors out of the question.
    let id = erase(&rig);
    let question = rig.notification("elicitation/create");
    rig.send(&json!({"jsonrpc": "2.0", "id": question["id"], "error": {"code": -32601, "message": "no forms here"}}));
    let refused = rig.response(id);
    assert!(is_error(&refused["result"]) && text(&refused["result"]).contains("no forms here"), "{refused}");

    // A response to a question nobody asked is not a request, and is dropped.
    rig.send(&json!({"jsonrpc": "2.0", "id": "simee-confirm-99", "result": {"action": "accept"}}));
    assert!(rig.quiet());
    assert_eq!(rig.fake.called("erase").len(), 1);
}

#[test]
fn a_question_nobody_answers_is_a_refusal() {
    let rig = rig(Policy::default());
    rig.server.ask_clients_to_confirm(Duration::ZERO);
    let init = rig.send_raw("initialize", json!({"protocolVersion": "2025-11-25", "capabilities": {"elicitation": {}}}));
    rig.response(init);
    let id = rig.send_raw("tools/call", json!({"name": "erase", "arguments": {"device": "ci-1", "confirm": true}}));
    let refused = rig.response(id);
    assert!(is_error(&refused["result"]) && text(&refused["result"]).contains("no answer came"), "{refused}");
    assert!(rig.fake.called("erase").is_empty());
}

#[test]
fn a_connection_that_cannot_carry_a_question_falls_back_to_the_confirm_argument() {
    // The default for a server a transport didn't enable (HTTP): the question can't be answered.
    let rig = rig(Policy::default());
    let init = rig.send_raw("initialize", json!({"protocolVersion": "2025-11-25", "capabilities": {"elicitation": {}}}));
    rig.response(init);
    let id = rig.send_raw("tools/call", json!({"name": "erase", "arguments": {"device": "ci-1"}}));
    let refused = rig.response(id);
    assert!(text(&refused["result"]).contains("can't carry a question"), "{refused}");
    assert!(rig.quiet(), "and no question was sent");
    let id = rig.send_raw("tools/call", json!({"name": "erase", "arguments": {"device": "ci-1", "confirm": true}}));
    assert!(!is_error(&rig.response(id)["result"]));
}

#[test]
fn shutting_down_what_this_client_booted_needs_no_confirmation_and_anything_else_does() {
    let (rig, world) = rig_world(Policy::default(), None);
    // ci-1 is shut down: booting it makes it this client's, and ending it is too.
    assert!(!is_error(&call(&rig, "boot", json!({"device": "ci-1"}))));
    assert!(!is_error(&call(&rig, "shutdown", json!({"device": "ci-1"}))));
    assert_eq!(rig.fake.called("shutdown"), [json!({"device": "ci-1"})]);

    // Someone else boots it: no longer this client's.
    world.set_state(CI_1, "Booted");
    let refused = call(&rig, "shutdown", json!({"device": "ci-1"}));
    assert!(is_error(&refused) && text(&refused).contains("which this client didn't boot"), "{refused}");
    // ci-2 was running before this client asked: asking for a boot of it doesn't make it theirs,
    // whichever way they name it.
    assert!(!is_error(&call(&rig, "boot", json!({"device": "ci-2"}))));
    for query in ["ci-2", CI_2] {
        let refused = call(&rig, "shutdown", json!({"device": query}));
        assert!(is_error(&refused) && text(&refused).contains("which this client didn't boot"), "{query}: {refused}");
    }
    // Nor is what one client booted another's.
    assert!(!is_error(&call(&rig, "boot_many", json!({"devices": ["ci-1", "prod-1"]}))));
    let other = call_as(&rig, "someone-else", json!({}), "shutdown", json!({"device": "ci-1"}));
    assert!(is_error(&other) && text(&other).contains("confirmation"), "{other}");
    assert_eq!(rig.fake.called("shutdown").len(), 1, "only the first ran");
}

#[test]
fn tools_that_only_sometimes_destroy_confirm_only_when_they_do() {
    let (rig, _) = rig_world(Policy::default(), None);
    // Setting and keeping a preference, and a console command that stops nothing, are not asked about.
    for (tool, args) in [
        ("prefs_batch", json!({"device": "ci-2", "bundle_id": "b", "store": "visible", "ops": [{"key": "a", "type": "string", "value": "1"}]})),
        ("prefs_batch", json!({"device": "ci-2", "bundle_id": "b", "store": "visible", "ops": [{"key": "a", "delete": false}]})),
        ("console", json!({"device": "ci-2", "command": "avd snapshot list"})),
        ("console", json!({"device": "ci-2", "command": "network status"})),
    ] {
        let result = call(&rig, tool, args);
        assert!(!is_error(&result), "{tool}: {result}");
    }
    // A client that booted the emulator ends it through the console as it would with `shutdown`.
    assert!(!is_error(&call(&rig, "boot", json!({"device": "ci-1"}))));
    assert!(!is_error(&call(&rig, "console", json!({"device": "ci-1", "command": "kill"}))));
    // And that put it back: ci-1 is nobody's again, so the same command asks.
    let again = call(&rig, "console", json!({"device": "ci-1", "command": "kill"}));
    assert!(is_error(&again) && text(&again).contains("needs the user's confirmation"), "{again}");
    // `confirm: true` doesn't reach the method either way.
    assert!(!is_error(&call(&rig, "console", json!({"device": "ci-1", "command": "kill", "confirm": true}))));
    assert!(rig.fake.called("console").iter().all(|p| p.get("confirm").is_none()));
}

// ---- policy --------------------------------------------------------------------------------

#[test]
fn a_client_cannot_erase_a_device_it_was_not_allowed_to_touch() {
    let rig = rig(devices_only(&["ci-*"]));
    for query in ["prod-1", PROD, "booted-but-not-yours"] {
        // Even with the confirmation in hand.
        let result = call(&rig, "erase", json!({"device": query, "confirm": true}));
        assert!(is_error(&result), "{query}: {result}");
        assert!(text(&result).starts_with("refused: ") || text(&result).contains("no device matches"), "{query}: {}", text(&result));
    }
    let refused = call(&rig, "erase", json!({"device": "prod-1", "confirm": true}));
    assert!(
        text(&refused).contains("may not touch") && text(&refused).contains("`prod-1` (UDID-PROD)") && text(&refused).contains("ci-*"),
        "{}",
        text(&refused)
    );
    assert!(rig.fake.called("erase").is_empty(), "the device was never asked");

    // What it is allowed is by name or by id.
    assert!(!is_error(&call(&rig, "erase", json!({"device": "ci-1", "confirm": true}))));
    assert!(!is_error(&call(&rig, "erase", json!({"device": CI_2, "confirm": true}))));
    assert_eq!(rig.fake.called("erase").len(), 2);

    // And each other tool is held to it the same: the policy isn't about erase.
    for (tool, args) in [
        ("tap", json!({"device": "prod-1", "x": 1, "y": 1})),
        ("screenshot", json!({"device": PROD})),
        ("boot_many", json!({"devices": ["ci-1", "prod-1"]})),
        ("clone", json!({"device": "prod-1", "name": "ci-copy"})),
    ] {
        let result = call(&rig, tool, args);
        assert!(is_error(&result) && text(&result).contains("may not touch"), "{tool}: {result}");
        assert!(rig.fake.called(tool).is_empty(), "{tool}");
    }
    // `booted` is whichever device is booted: the first one is ci-2, which is allowed.
    assert!(!is_error(&call(&rig, "tap", json!({"x": 1, "y": 1}))));
    // The device it hasn't been told of doesn't show up in what it lists.
    let listed = call(&rig, "devices", json!({}));
    let names: Vec<&str> = listed["structuredContent"].as_array().expect("devices").iter().filter_map(|d| d["name"].as_str()).collect();
    assert_eq!(names, ["ci-1", "ci-2"]);
}

#[test]
fn a_camera_cannot_show_a_screen_the_client_may_not_touch() {
    let rig = rig(devices_only(&["ci-*"]));
    let link = |source: &str| call(&rig, "camera_link", json!({"device": "ci-1", "facing": "both", "source": source}));
    // Another device's screen, named any way the source id allows: by name, by UDID, with a crop, in
    // another case, with space around it.
    for source in ["device:prod-1", &format!("device:{PROD}"), "device:prod-1:0,0,100,100", "DEVICE:prod-1", "  device:prod-1 "] {
        let refused = link(source);
        assert!(is_error(&refused) && text(&refused).contains("may not touch") && text(&refused).contains("prod-1"), "{source}: {refused}");
    }
    assert!(rig.fake.called("camera_link").is_empty(), "no link was made");
    // A screen it may touch, and sources that show no device's, are fine.
    for source in ["device:ci-2", "pattern:testcard", "qr:device:prod-1"] {
        assert!(!is_error(&link(source)), "{source}");
    }
    // A boot's cameras are the same thing under other names.
    for key in ["camera_back", "camera_front"] {
        let refused = call(&rig, "boot", json!({"device": "ci-1", key: "device:prod-1"}));
        assert!(is_error(&refused) && text(&refused).contains("may not touch"), "{key}: {refused}");
    }
    assert!(rig.fake.called("boot").is_empty());
    assert!(!is_error(&call(&rig, "boot", json!({"device": "ci-1", "camera_back": "device:ci-2"}))));
}

#[test]
fn a_stream_is_judged_by_the_device_it_shows() {
    let rig = rig(devices_only(&["ci-*"]));
    rig.fake.answer("streams", Ok(json!([{"stream_id": "stream-1", "device": PROD}, {"stream_id": "stream-2", "device": CI_2}])));
    for tool in ["stream_status", "stream_stop"] {
        let refused = call(&rig, tool, json!({"stream_id": "stream-1"}));
        assert!(is_error(&refused) && text(&refused).contains("may not touch"), "{tool}: {refused}");
        assert!(!is_error(&call(&rig, tool, json!({"stream_id": "stream-2"}))), "{tool}");
        // One that isn't there is the method's to say.
        assert!(!is_error(&call(&rig, tool, json!({"stream_id": "stream-9"}))));
    }
    assert_eq!(rig.fake.called("stream_stop"), [json!({"stream_id": "stream-2"}), json!({"stream_id": "stream-9"})]);
}

#[test]
fn a_cursor_is_the_clients_own_and_its_next_page_is_judged_again() {
    let (rig, world) = rig_world(devices_only(&[crate::policy::CREATED]), None);
    rig.fake.answer("processes", Ok(json!([1, 2, 3, 4, 5])));
    let made = call(&rig, "create", json!({"name": "mine", "device_type": "t", "runtime": "r"}));
    assert!(!is_error(&made), "{made}");
    let first = call(&rig, "processes", json!({"device": "mine", "limit": 2}));
    let cursor = first["structuredContent"]["next_cursor"].clone();
    assert!(cursor.is_string(), "{first}");
    // Another client on the same server can't turn a page of a list it was never given.
    let stolen = call_as(&rig, "another-agent", json!({}), "processes", json!({"cursor": cursor}));
    assert!(is_error(&stolen) && text(&stolen).contains("`cursor` has expired"), "{stolen}");
    assert!(!is_error(&call(&rig, "processes", json!({"cursor": cursor}))));
    // The device stops being this client's (deleted): the rest of its list isn't either.
    world.devices.lock().expect("devices").retain(|d| d["name"] != "mine");
    let gone = call(&rig, "processes", json!({"cursor": cursor}));
    assert!(is_error(&gone), "{gone}");
}

#[test]
fn what_a_client_may_not_touch_is_not_in_its_resources_either() {
    let rig = rig(devices_only(&["ci-*"]));
    let read = |uri: &str| {
        let id = rig.send_raw("resources/read", json!({"uri": uri, "_meta": meta("agent", json!({}))}));
        rig.response(id)
    };
    let listed = read("simee://devices");
    let text = listed["result"]["contents"][0]["text"].as_str().expect("text");
    assert!(text.contains("ci-1") && !text.contains("prod-1"), "{text}");
    let one = read(&format!("simee://devices/{PROD}"));
    assert!(one["error"]["message"].as_str().is_some_and(|m| m.contains("may not touch")), "{one}");
}

#[test]
fn a_device_made_by_a_client_is_its_own_and_nobody_elses() {
    let rig = rig(devices_only(&[crate::policy::CREATED]));
    // Nothing yet.
    let refused = call(&rig, "tap", json!({"device": "ci-1", "x": 1, "y": 1}));
    assert!(is_error(&refused) && text(&refused).contains("may not touch"), "{refused}");
    assert_eq!(call(&rig, "devices", json!({}))["structuredContent"], json!([]));

    let made = call(&rig, "create", json!({"name": "mine", "device_type": "t", "runtime": "r"}));
    assert!(!is_error(&made), "{made}");
    assert!(!is_error(&call(&rig, "tap", json!({"device": "mine", "x": 1, "y": 1}))));
    assert!(!is_error(&call(&rig, "erase", json!({"device": "mine", "confirm": true}))));
    let names = |result: &Value| -> Vec<String> {
        result["structuredContent"].as_array().expect("devices").iter().filter_map(|d| d["name"].as_str().map(str::to_owned)).collect()
    };
    assert_eq!(names(&call(&rig, "devices", json!({}))), ["mine"]);
    // Not the others', and not ci-1 after creating it either.
    assert!(is_error(&call(&rig, "erase", json!({"device": "prod-1", "confirm": true}))));
    let theirs = call_as(&rig, "another-agent", json!({}), "tap", json!({"device": "mine", "x": 1, "y": 1}));
    assert!(is_error(&theirs) && text(&theirs).contains("may not touch"), "{theirs}");

    // Deleting it takes it off the list with the rest.
    assert!(!is_error(&call(&rig, "delete", json!({"device": "mine", "confirm": true}))));
    assert_eq!(names(&call(&rig, "devices", json!({}))), Vec::<String>::new());
}

#[test]
fn a_name_the_client_could_not_touch_afterwards_is_not_made() {
    let rig = rig(devices_only(&["ci-*"]));
    let refused = call(&rig, "create", json!({"name": "other-1", "device_type": "t", "runtime": "r"}));
    assert!(is_error(&refused) && text(&refused).contains("couldn't touch afterwards"), "{refused}");
    assert!(rig.fake.called("create").is_empty());
    assert!(!is_error(&call(&rig, "create", json!({"name": "ci-new", "device_type": "t", "runtime": "r"}))));
}

#[test]
fn a_listing_that_names_no_device_is_refused_where_it_would_name_them_all() {
    let rig = rig(devices_only(&["ci-*"]));
    let refused = call(&rig, "streams", json!({}));
    assert!(is_error(&refused) && text(&refused).contains("name a device"), "{refused}");
    assert!(!is_error(&call(&rig, "streams", json!({"device": "ci-1"}))));
    // Without a policy on devices the listing is what it always was.
    assert!(!is_error(&call(&self::rig(Policy::default()), "streams", json!({}))));
}

#[test]
fn a_read_only_client_can_look_and_nothing_else() {
    let policy = Policy::parse(r#"{"default": {"read_only": true}, "clients": {"writer": {"read_only": false}}}"#).expect("policy");
    let rig = rig(policy);
    // It isn't offered what it can't use.
    let id = rig.send_raw("tools/list", json!({"_meta": meta("agent", json!({}))}));
    let listed = rig.response(id);
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|t| t["name"] == "devices") && tools.iter().any(|t| t["name"] == "screenshot"));
    assert!(tools.iter().all(|t| t["annotations"]["readOnlyHint"] == true), "only read-only tools are listed");
    assert!(!tools.iter().any(|t| t["name"] == "tap"));

    // Calling one anyway is refused, and says how to allow it.
    let refused = call(&rig, "tap", json!({"device": "ci-2", "x": 1, "y": 1}));
    assert!(is_error(&refused), "{refused}");
    assert!(text(&refused).contains("read-only") && text(&refused).contains("--read-only"), "{}", text(&refused));
    assert!(rig.fake.called("tap").is_empty());
    for (tool, args) in [
        ("erase", json!({"device": "ci-1", "confirm": true})),
        ("boot", json!({"device": "ci-1"})),
        ("exec", json!({"device": "ci-2", "program": "ls"})),
    ] {
        assert!(is_error(&call(&rig, tool, args)), "{tool}");
        assert!(rig.fake.called(tool).is_empty(), "{tool}");
    }
    // What it can: look.
    assert!(!is_error(&call(&rig, "devices", json!({}))));
    assert!(!is_error(&call(&rig, "screen", json!({"device": "ci-2"}))));
    // Another client's own rules override the default.
    assert!(!is_error(&call_as(&rig, "writer", json!({}), "tap", json!({"device": "ci-2", "x": 1, "y": 1}))));
    assert_eq!(rig.fake.called("tap").len(), 1);
}

#[test]
fn a_client_gets_only_the_toolsets_its_policy_names() {
    let policy = Policy::parse(r#"{"clients": {"viewer": {"toolsets": ["observe"]}}}"#).expect("policy");
    let rig = rig(policy);
    let id = rig.send_raw("tools/list", json!({"_meta": meta("viewer", json!({}))}));
    let listed = rig.response(id);
    let names: Vec<&str> = listed["result"]["tools"].as_array().expect("tools").iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"screenshot") && names.contains(&"logs"), "{names:?}");
    assert!(!names.contains(&"tap") && !names.contains(&"boot") && !names.contains(&"exec"), "{names:?}");

    let refused = rig.send_raw(
        "tools/call",
        json!({"name": "tap", "arguments": {"device": "ci-2", "x": 1, "y": 1}, "_meta": meta("viewer", json!({}))}),
    );
    let refused = rig.response(refused);
    assert_eq!(refused["error"]["code"], rpc::INVALID_PARAMS);
    let message = refused["error"]["message"].as_str().expect("message");
    assert!(message.contains("isn't allowed for this client") && message.contains("observe") && message.contains("--policy"), "{message}");
    assert!(rig.fake.called("tap").is_empty());
    // Everyone else has all of them.
    assert!(!is_error(&call(&rig, "tap", json!({"device": "ci-2", "x": 1, "y": 1}))));
}

#[test]
fn the_transports_name_for_a_client_beats_the_one_it_declares() {
    let policy = Policy::parse(r#"{"clients": {"ops": {"read_only": false}, "viewer": {"read_only": true}}}"#).expect("policy");
    let rig = rig(policy);
    rig.server.set_identity("ops");
    // It says it's the read-only viewer; the token said otherwise.
    let result = call_as(&rig, "viewer", json!({}), "tap", json!({"device": "ci-2", "x": 1, "y": 1}));
    assert!(!is_error(&result), "{result}");
}

// ---- admission -----------------------------------------------------------------------------

#[test]
fn boots_go_through_the_hosts_admission_and_the_verdict_comes_back() {
    let rig = rig(Policy::default());
    rig.fake.answer("boot", Ok(json!({"boot_s": 3.0, "admission": {"verdict": "admit", "reasons": ["room"]}})));
    let booted = call(&rig, "boot", json!({"device": "ci-1"}));
    assert_eq!(rig.fake.called("boot"), [json!({"device": "ci-1", "admission": "refuse"})], "refuse unless the agent asks for more care");
    assert_eq!(booted["structuredContent"]["admission"]["verdict"], "admit", "the verdict is in the result");
    assert_eq!(text(&booted), r#"{"boot_s":3.0} (admit)"#);

    // The host can't take it: the method fails fast, and the agent reads why.
    rig.fake.answer("boot", Err("admission hold: memory used is 88% (nothing was done; admission \"ignore\" goes ahead anyway)".into()));
    let held = call(&rig, "boot", json!({"device": "ci-2"}));
    assert!(is_error(&held) && text(&held).contains("admission hold: memory used is 88%"), "{held}");

    // It can wait for room, never skip the check.
    rig.fake.answer("boot", Ok(json!({})));
    assert!(!is_error(&call(&rig, "boot", json!({"device": "ci-2", "admission": "wait"}))));
    assert!(!is_error(&call(&rig, "boot_many", json!({"devices": ["ci-1"], "admission": "wait"}))));
    assert_eq!(rig.fake.called("boot_many"), [json!({"devices": ["ci-1"], "admission": "wait"})]);
    for (tool, args) in [
        ("boot", json!({"device": "ci-1"})),
        ("boot_many", json!({"devices": ["ci-1"]})),
        ("create", json!({"name": "n"})),
        ("clone", json!({"device": "ci-1", "name": "n"})),
    ] {
        let mut skipping = args.clone();
        skipping["admission"] = "ignore".into();
        let refused = call(&rig, tool, skipping);
        assert!(is_error(&refused) && text(&refused).contains("`ignore` is the operator's to allow"), "{tool}: {refused}");
        // And without a word on it, it is applied.
        let before = rig.fake.called(tool).len();
        call(&rig, tool, args);
        assert_eq!(rig.fake.called(tool).len(), before + 1);
        assert_eq!(rig.fake.called(tool).last().expect("call")["admission"], "refuse", "{tool}");
    }
}

#[test]
fn an_operator_can_turn_admission_off_for_a_server() {
    let policy = Policy::default().with_default(|p| p.admission = crate::policy::AdmissionMode::Ignore);
    let rig = rig(policy);
    assert!(!is_error(&call(&rig, "boot", json!({"device": "ci-1", "admission": "ignore"}))));
    assert!(!is_error(&call(&rig, "boot", json!({"device": "ci-2"}))));
    assert_eq!(rig.fake.called("boot"), [json!({"device": "ci-1", "admission": "ignore"}), json!({"device": "ci-2"})]);
}

// ---- audit ---------------------------------------------------------------------------------

fn log_in(dir: &Scratch) -> (AuditLog, std::path::PathBuf) {
    let path = dir.0.join("audit.log");
    (AuditLog::open(audit::Config::new(path.clone())).expect("audit log"), path)
}

fn entries(path: &std::path::Path) -> Vec<Value> {
    audit::query(path, &audit::Query { limit: Some(1000), ..audit::Query::default() }).expect("query")
}

#[test]
fn the_audit_log_records_every_call_and_keeps_secrets_out_of_it() {
    let dir = Scratch::new("audit-calls");
    let (log, path) = log_in(&dir);
    let rig = rig_logging(Policy::default(), Some(log));
    let secret = "s3cr3t-value-123";
    call(&rig, "devices", json!({}));
    call_as(
        &rig,
        "claude-code",
        json!({}),
        "exec",
        json!({"device": "ci-2", "program": "curl", "args": ["-sS", format!("https://user:{secret}@example.com/x?token={secret}")],
            "env": {"API_TOKEN": secret, "HOME": "/root"}}),
    );
    call(&rig, "type", json!({"device": "ci-2", "text": secret}));
    call(&rig, "stream_start", json!({"device": "ci-2", "whip_url": "https://example.com/whip", "token": secret}));
    call(&rig, "erase", json!({"device": "ci-1"}));
    call(&rig, "erase", json!({"device": "ci-1", "confirm": true}));
    call(&rig, "tap", json!({"device": "prod-1-doesnt-exist", "x": 1, "y": 1}));
    // A tool that isn't there, a toolset that isn't served, and one that fails.
    let unknown = rig.send_raw("tools/call", json!({"name": "teleport", "arguments": {"to": "mars"}, "_meta": meta("agent", json!({}))}));
    rig.response(unknown);
    rig.fake.answer("screen", Err("device is gone".into()));
    call(&rig, "screen", json!({"device": "ci-2"}));

    let log = entries(&path);
    let seen: Vec<(&str, &str, &str)> = log
        .iter()
        .map(|e| (e["client"].as_str().unwrap_or(""), e["tool"].as_str().unwrap_or(""), e["status"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(
        seen,
        [
            ("agent", "devices", "ok"),
            ("claude-code", "exec", "ok"),
            ("agent", "type", "ok"),
            ("agent", "stream_start", "ok"),
            ("agent", "erase", "refused"),
            ("agent", "erase", "ok"),
            ("agent", "tap", "ok"),
            ("agent", "teleport", "refused"),
            ("agent", "screen", "error"),
        ],
        "one entry per call, in order, with who, what and how it ended"
    );
    assert!(log.iter().all(|e| e["ts_ms"].as_u64().is_some_and(|t| t > 0) && e["duration_ms"].is_u64()));
    let raw = std::fs::read_to_string(&path).expect("log");
    assert!(!raw.contains(secret), "a secret reached the log:\n{raw}");
    // What is left says what happened.
    let exec = &log[1]["args"];
    assert_eq!(exec["env"]["API_TOKEN"], "[redacted]");
    assert_eq!(exec["env"]["HOME"], "/root");
    assert_eq!(exec["program"], "curl");
    assert!(exec["args"][1].as_str().is_some_and(|a| a.starts_with("https://[redacted]@example.com/x?token=[redacted]")), "{exec}");
    assert!(log[2]["args"]["text"].as_str().is_some_and(|t| t.starts_with("[redacted] (16 bytes)")), "{}", log[2]);
    assert_eq!(log[3]["args"]["token"], "[redacted]");
    assert!(log[4]["detail"].as_str().is_some_and(|d| d.contains("needs the user's confirmation")));
    assert_eq!(log[5]["args"], json!({"device": "ci-1", "confirm": true}), "the arguments are the client's");
    assert!(log[8]["detail"].as_str().is_some_and(|d| d.contains("device is gone")));
}

#[test]
fn a_task_a_confirmation_to_come_and_a_policy_refusal_are_logged_too() {
    let dir = Scratch::new("audit-more");
    let (log, path) = log_in(&dir);
    let rig = rig_logging(devices_only(&["ci-*"]), Some(log));
    rig.fake.answer("boot", Ok(json!({"boot_s": 1.0, "admission": {"verdict": "admit"}})));
    // A task: logged when it finishes, as it ran.
    let created = rig.send_raw(
        "tools/call",
        json!({"name": "boot", "arguments": {"device": "ci-1"}, "_meta": meta("agent", json!({"extensions": {EXT_TASKS: {}}}))}),
    );
    let created = rig.response(created);
    let id = created["result"]["taskId"].as_str().expect("task").to_owned();
    while rig.modern("tasks/get", json!({"taskId": id}))["result"]["status"] != "completed" {
        std::thread::yield_now();
    }
    // Asked, not yet confirmed.
    let first = call_as(&rig, "agent", asks(), "erase", json!({"device": "ci-1"}));
    assert_eq!(first["resultType"], "input_required");
    // Not its device.
    call(&rig, "tap", json!({"device": "prod-1", "x": 1, "y": 1}));

    let log = entries(&path);
    let seen: Vec<(&str, &str)> = log.iter().map(|e| (e["tool"].as_str().unwrap_or(""), e["status"].as_str().unwrap_or(""))).collect();
    assert_eq!(seen, [("boot", "ok"), ("erase", "input_required"), ("tap", "refused")]);
    assert_eq!(log[0]["admission"], "admit", "the verdict a boot came back with");
    assert!(log[2]["detail"].as_str().is_some_and(|d| d.contains("may not touch")));
}
