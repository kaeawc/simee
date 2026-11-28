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
    assert_eq!(device_queries("create", &json!({"device": "pixel_fold", "name": "n"})), Vec::<String>::new());
    assert_eq!(device_queries("tap", &json!({"device": 7})), Vec::<String>::new(), "the method's own error says so");
    // Every tool that confirms takes the device it confirms about.
    for tool in CONFIRM {
        assert_eq!(device_param(tool), Some(true), "{tool}");
    }
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
