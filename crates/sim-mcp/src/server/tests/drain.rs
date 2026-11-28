//! Shutting down (#388) through the protocol: once the drain begins a tool call is answered
//! `cancelled: server shutting down` and not run, the calls in flight are waited for (their answers
//! are sent before the drain is done) and a question waiting for the user is dropped. Nothing here
//! waits on a clock: each step waits for the state it needs, with a deadline for a hang.

use super::*;
use crate::policy::Policy;

/// A rig whose server counts its calls with a drain of its own (not the process's, which a test
/// must never begin).
fn rig() -> (Rig, Arc<Drain>) {
    let drain = Arc::new(Drain::default());
    (Rig::draining(Toolsets::all(), |fake| fake, Arc::new(Safety::default()), drain.clone()), drain)
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or("")
}

/// Waits until the fake has started a call of `method`.
fn until_called(rig: &Rig, method: &str) {
    let until = Instant::now() + WAIT;
    while rig.fake.called(method).is_empty() {
        assert!(Instant::now() < until, "`{method}` never started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until the drain has begun, on a thread that finishes it with `limit`.
fn finishing(drain: &Arc<Drain>, limit: Duration) -> std::thread::JoinHandle<bool> {
    let theirs = drain.clone();
    let handle = std::thread::spawn(move || theirs.finish(limit));
    let until = Instant::now() + WAIT;
    while !drain.draining() {
        assert!(Instant::now() < until, "the drain never began");
        std::thread::sleep(Duration::from_millis(5));
    }
    handle
}

#[test]
fn a_call_made_while_draining_is_answered_cancelled_and_not_run() {
    let (rig, drain) = rig();
    assert!(drain.finish(Duration::ZERO), "nothing in flight");
    for tasks in [false, true] {
        let id = rig.send_modern("tools/call", json!({"name": "tap", "arguments": {"device": "phone", "x": 1, "y": 2}}), tasks);
        let result = rig.response(id)["result"].clone();
        assert_eq!(result["isError"], true, "{result}");
        assert!(text(&result).contains("cancelled: server shutting down"), "{result}");
    }
    assert!(rig.fake.called("tap").is_empty(), "the device never saw it");
    // What isn't a tool call (a listing, a resource) is still answered, until the process exits.
    assert!(rig.modern("tools/list", json!({}))["result"]["tools"].is_array());
}

#[test]
fn the_drain_waits_for_a_call_in_flight_and_the_call_is_answered_not_cancelled() {
    let (rig, drain) = rig();
    let gate = rig.fake.gate("tap");
    let id = rig.send_modern("tools/call", json!({"name": "tap", "arguments": {"device": "phone", "x": 1, "y": 2}}), false);
    until_called(&rig, "tap");
    let waiting = finishing(&drain, WAIT);
    assert!(!waiting.is_finished(), "the call is held, so the drain waits");
    gate.release();
    assert!(waiting.join().expect("joins"), "the call ended within the bound");
    // (That the reply is sent before the wait ends is `simee mcp`'s own test: its client reads the
    // reply, then sees the exit.)
    let reply = rig.response(id);
    assert_ne!(reply["result"]["isError"], true, "a call that finished is not cancelled: {reply}");
}

#[test]
fn a_call_that_ends_in_error_after_the_drain_began_says_the_shutdown_cancelled_it() {
    let (rig, drain) = rig();
    rig.fake.answer("tap", Err("the device went away".into()));
    let gate = rig.fake.gate("tap");
    let id = rig.send_modern("tools/call", json!({"name": "tap", "arguments": {"device": "phone", "x": 1, "y": 2}}), false);
    until_called(&rig, "tap");
    let waiting = finishing(&drain, WAIT);
    gate.release();
    assert!(waiting.join().expect("joins"));
    let result = rig.response(id)["result"].clone();
    assert!(text(&result).contains("cancelled: server shutting down (the device went away)"), "{result}");
}

#[test]
fn the_drain_gives_up_on_a_call_that_does_not_end_within_its_bound() {
    let (rig, drain) = rig();
    let gate = rig.fake.gate("tap");
    rig.send_modern("tools/call", json!({"name": "tap", "arguments": {"device": "phone", "x": 1, "y": 2}}), false);
    until_called(&rig, "tap");
    assert!(!drain.finish(Duration::from_millis(50)), "still held when the bound passed");
    gate.release();
}

#[test]
fn a_task_counts_until_it_ends() {
    let (rig, drain) = rig();
    let gate = rig.fake.gate("boot");
    let created = rig.with_tasks("tools/call", json!({"name": "boot", "arguments": {"device": "phone"}}));
    assert_eq!(created["result"]["resultType"], "task", "{created}");
    until_called(&rig, "boot");
    let waiting = finishing(&drain, WAIT);
    assert!(!waiting.is_finished(), "the task's call is held, though its request was answered");
    gate.release();
    assert!(waiting.join().expect("joins"));
}

#[test]
fn a_question_waiting_for_the_users_answer_is_dropped_when_the_drain_begins() {
    let safety = Arc::new(Safety::new(Policy::default(), None));
    let drain = Arc::new(Drain::default());
    let rig = Rig::draining(Toolsets::all(), |fake| fake, safety, drain.clone());
    rig.server.ask_clients_to_confirm(WAIT);
    let init = rig.send_raw(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {"elicitation": {}}, "clientInfo": {"name": "old", "version": "1"}}),
    );
    rig.response(init);
    let id = rig.send_raw("tools/call", json!({"name": "erase", "arguments": {"device": "phone", "confirm": true}}));
    rig.notification("elicitation/create");
    let waiting = finishing(&drain, WAIT);
    let refused = rig.response(id)["result"].clone();
    assert!(waiting.join().expect("joins"), "the question ended, so the call did");
    assert_eq!(refused["isError"], true);
    assert!(text(&refused).contains("cancelled: server shutting down"), "{refused}");
    assert!(rig.fake.called("erase").is_empty(), "nothing was erased");
}
