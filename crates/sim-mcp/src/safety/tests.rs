use serde_json::json;

use super::*;

#[test]
fn an_answer_is_yes_only_for_an_accepted_form_that_says_yes() {
    assert_eq!(Confirmation::read(&json!({"action": "accept", "content": {"confirm": true}})), Answer::Yes);
    for (answer, says) in [
        (json!({"action": "accept", "content": {"confirm": false}}), "without allowing"),
        (json!({"action": "accept", "content": {"confirm": "true"}}), "without allowing"),
        (json!({"action": "accept", "content": {"other": true}}), "without allowing"),
        (json!({"action": "accept"}), "without allowing"),
        (json!({"action": "decline"}), "declined"),
        (json!({"action": "cancel"}), "dismissed"),
        (json!({"action": "yes"}), "no usable answer"),
        (json!({}), "no usable answer"),
        (json!("accept"), "no usable answer"),
    ] {
        match Confirmation::read(&answer) {
            Answer::No(reason) => assert!(reason.contains(says), "{answer}: {reason}"),
            Answer::Yes => panic!("{answer} is not a yes"),
        }
    }
}

#[test]
fn the_state_of_a_question_is_for_one_client_one_tool_and_one_set_of_arguments() {
    let safety = Safety::default();
    let args = json!({"device": "a"});
    let state = safety.seal_state("agent", "erase", &args);
    assert_eq!(state, safety.seal_state("agent", "erase", &args), "the same call, the same state");
    assert_ne!(state, safety.seal_state("other", "erase", &args));
    assert_ne!(state, safety.seal_state("agent", "delete", &args));
    assert_ne!(state, safety.seal_state("agent", "erase", &json!({"device": "b"})));
    assert_ne!(state, Safety::default().seal_state("agent", "erase", &args), "and a key of this process's own");
}

#[test]
fn which_tools_name_which_devices() {
    // Required, optional and none; `create`'s `device` is a hardware profile.
    assert_eq!(device_param("tap"), Some(true));
    assert_eq!(device_param("streams"), Some(false));
    assert_eq!(device_param("camera_links"), Some(false));
    assert_eq!(device_param("create"), None);
    assert_eq!(device_param("devices"), None);
    assert_eq!(device_param("boot_many"), None, "it names `devices`");
    assert_eq!(device_param(VIEW_TOOL), Some(false));

    assert_eq!(device_queries("tap", &json!({"device": "a"})), ["a"]);
    assert_eq!(device_queries("tap", &json!({})), ["booted"], "the API's default");
    assert_eq!(device_queries("device_view", &json!({})), ["booted"]);
    assert_eq!(device_queries("streams", &json!({})), Vec::<String>::new(), "every device's");
    assert_eq!(device_queries("streams", &json!({"device": "a"})), ["a"]);
    assert_eq!(device_queries("boot_many", &json!({"devices": ["a", "b", 3]})), ["a", "b"]);
    // A link's members; one named by id says no device here (`linked_members` reads them).
    assert_eq!(device_param("link"), None, "it names `devices`");
    assert_eq!(device_queries("link", &json!({"devices": ["watch", "phone"]})), ["watch", "phone"]);
    assert_eq!(device_queries("link", &json!({"link": "ID"})), Vec::<String>::new());
    assert_eq!(device_queries("unlink", &json!({"device": "phone"})), ["phone"]);
    assert_eq!(device_queries("unlink", &json!({"link": "ID"})), Vec::<String>::new());
    assert_eq!(device_queries("links", &json!({})), Vec::<String>::new(), "every device's, filtered after");
    assert_eq!(device_queries("create", &json!({"device": "pixel_fold", "name": "n"})), Vec::<String>::new());
    assert_eq!(device_queries("tap", &json!({"device": 7})), Vec::<String>::new(), "the method's own error says so");
    // Every tool that confirms takes the device it confirms about, but `unlink`, which names its
    // links by id or by a device and whose members `linked_members` finds.
    for tool in CONFIRM.iter().filter(|t| **t != "unlink") {
        assert_eq!(device_param(tool), Some(true), "{tool}");
    }
    assert_eq!(device_param("unlink"), Some(false));
}

/// Pairs as `links` returns them: one of `ci-1` and `ci-2`, one of `ci-2` and `prod-1`.
struct Linked;

impl Backend for Linked {
    fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
        let member = |udid: &str, name: &str| json!({"device": udid, "name": name, "running": true});
        match method {
            "links" => Ok(json!([
                {"id": "PAIR-A", "members": [member("U-CI-2", "ci-2"), member("U-CI-1", "ci-1")]},
                {"id": "PAIR-B", "members": [member("U-CI-2", "ci-2"), member("U-PROD", "prod-1")]},
            ])),
            "info" => {
                let q = params["device"].as_str().unwrap_or("");
                let udid = format!("U-{}", q.to_uppercase());
                Ok(json!({"device": {"udid": udid, "name": q, "state": "Booted"}}))
            }
            other => Err(format!("unexpected {other}")),
        }
    }
}

#[test]
fn a_link_by_id_or_by_device_names_every_member_of_the_links_it_touches() {
    let udids = |method: &str, params: Value| -> Result<Vec<String>, String> {
        linked_members(&Linked, method, &params).map(|m| m.into_iter().map(|d| d.0.udid).collect())
    };
    assert_eq!(udids("unlink", json!({"link": "pair-a"})), Ok(vec!["U-CI-2".to_owned(), "U-CI-1".to_owned()]), "ids in any case");
    assert_eq!(udids("link", json!({"link": "PAIR-B", "devices": ["ci-1"]})).expect("by id").len(), 2);
    assert_eq!(udids("link_status", json!({"device": "ci-1"})), Ok(vec!["U-CI-2".to_owned(), "U-CI-1".to_owned()]));
    assert_eq!(udids("unlink", json!({"device": "ci-2"})).expect("both pairs").len(), 4);
    assert!(udids("unlink", json!({"link": "PAIR-Z"})).expect_err("unknown id").contains("no link `PAIR-Z`"));
    // `link` by devices names them all itself; other methods have no links.
    assert_eq!(udids("link", json!({"devices": ["ci-1", "ci-2"], "device": "prod"})), Ok(Vec::new()));
    assert_eq!(udids("links", json!({"device": "ci-1"})), Ok(Vec::new()));
    assert_eq!(udids("tap", json!({"link": "PAIR-A"})), Ok(Vec::new()));
}

#[test]
fn every_confirming_tool_is_a_tool_that_changes_something() {
    for tool in CONFIRM {
        assert!(tools::exists(tool), "{tool}");
        assert!(!tools::read_only(tool), "{tool}");
        assert!(!confirm_sentence(tool).is_empty());
    }
    assert!(confirm_sentence("shutdown").contains("didn't boot") && confirm_sentence("erase").contains("every time"));
}

#[test]
fn a_call_is_filed_by_how_it_ended() {
    assert_eq!(outcome(&Ok(json!({"admission": {"verdict": "hold"}}))), (audit::status::OK, None, Some("hold".to_owned())));
    assert_eq!(outcome(&Ok(json!(null))), (audit::status::OK, None, None));
    assert_eq!(outcome(&Err(format!("{REFUSED}nope"))).0, audit::status::REFUSED);
    assert_eq!(outcome(&Err("admission backoff: memory".into())).0, audit::status::REFUSED);
    assert_eq!(outcome(&Err("device is gone".into())).0, audit::status::ERROR);
}

#[test]
fn unlinking_a_kept_link_says_nothing_is_erased() {
    let pair = [(Resolved { udid: "xr".into(), name: "xr".into(), running: true }, "headset".to_owned())];
    let kept = what_it_does("unlink", &json!({"link": "lnk-0123abcd"}), &[], None, &pair);
    assert_eq!(kept, "unlink `lnk-0123abcd` (headset `xr`): its forwards and each member's saved copy are removed");
    let watch = what_it_does("unlink", &json!({"link": "FA1A0000-0000-4000-8000-000000000001"}), &[], None, &[]);
    assert!(watch.ends_with("CoreSimulator may shut its watch down and erase it"), "{watch}");
}
