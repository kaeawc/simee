use serde_json::json;

use super::*;

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn star_and_question_mark_match_by_the_whole_text() {
    for (pattern, text, matches) in [
        ("ci-*", "ci-1", true),
        ("ci-*", "ci-", true),
        ("ci-*", "xci-1", false),
        ("*", "", true),
        ("*-phone", "pixel-phone", true),
        ("*-phone", "pixel-phone-2", false),
        ("a*b*c", "aXXbYYc", true),
        ("a*b*c", "aXXcYYb", false),
        ("ci-?", "ci-1", true),
        ("ci-?", "ci-12", false),
        ("exact", "exact", true),
        ("exact", "Exact", false),
        ("", "", true),
        ("", "x", false),
        ("*a", "ba", true),
        ("*a*", "xyz", false),
    ] {
        assert_eq!(glob(pattern, text), matches, "{pattern} against {text}");
    }
}

#[test]
fn a_device_is_allowed_by_its_id_its_name_or_by_having_been_made_by_the_client() {
    let mut policy = ClientPolicy::default();
    assert!(policy.allows_device("U1", "anything", &set(&[])), "no list, no limit");
    policy.devices = Some(vec!["ci-*".into(), "UDID-7".into(), CREATED.into()]);
    let made = set(&["U9"]);
    assert!(policy.allows_device("U1", "ci-phone", &made), "by name");
    assert!(policy.allows_device("UDID-7", "prod", &made), "by id");
    assert!(policy.allows_device("U9", "prod", &made), "made by this client");
    assert!(!policy.allows_device("U8", "prod", &made));
    policy.devices = Some(vec![]);
    assert!(!policy.allows_device("U1", "ci-1", &made), "an empty list allows nothing");
    // A device named `@created` is not every device the client made.
    policy.devices = Some(vec![CREATED.into()]);
    assert!(!policy.allows_device("U1", CREATED, &set(&[])));
}

#[test]
fn a_new_device_needs_a_name_the_client_may_touch_afterwards_or_to_own_what_it_makes() {
    let mut policy = ClientPolicy::default();
    assert!(policy.allows_new_name("whatever"));
    policy.devices = Some(vec!["ci-*".into()]);
    assert!(policy.allows_new_name("ci-new") && !policy.allows_new_name("prod"));
    policy.devices = Some(vec![CREATED.into()]);
    assert!(policy.allows_new_name("prod"));
}

#[test]
fn a_clients_rules_are_the_defaults_with_its_own_fields_on_top() {
    let policy = Policy::parse(
        r#"{
            "default": {"read_only": true, "devices": ["ci-*"], "toolsets": ["core", "debug"], "admission": "refuse"},
            "clients": {
                "writer": {"read_only": false},
                "ops": {"devices": ["*"], "admission": "ignore", "confirm": ["uninstall"]}
            }
        }"#,
    )
    .expect("policy");
    let unknown = policy.for_client("nobody");
    assert!(unknown.read_only && unknown.devices == Some(vec!["ci-*".into()]) && unknown.admission == AdmissionMode::Refuse);
    assert!(unknown.toolsets.as_ref().is_some_and(|t| t.contains("exec") && t.contains("tap") && !t.contains("rm")));
    let writer = policy.for_client("writer");
    assert!(!writer.read_only && writer.devices == unknown.devices, "only what it names changes");
    let ops = policy.for_client("ops");
    assert!(ops.read_only);
    assert_eq!((ops.devices, ops.admission, ops.confirm_also), (Some(vec!["*".into()]), AdmissionMode::Ignore, set(&["uninstall"])));
    assert_eq!(Policy::parse("{}").expect("empty"), Policy::default());
}

#[test]
fn a_policy_that_does_not_parse_says_where_instead_of_loosening() {
    for (text, says) in [
        ("[]", "one JSON object"),
        ("nope", "not JSON"),
        (r#"{"defaults": {}}"#, "unknown key `defaults`"),
        (r#"{"default": {"readonly": true}}"#, "unknown key `default.readonly`"),
        (r#"{"clients": {"x": {"read_only": "yes"}}}"#, "`clients.x.read_only` is true or false"),
        (r#"{"clients": []}"#, "`clients` is an object"),
        (r#"{"default": {"devices": "ci-*"}}"#, "`default.devices` is a list of strings"),
        (r#"{"default": {"devices": [1]}}"#, "list of strings"),
        (r#"{"default": {"toolsets": ["nope"]}}"#, "unknown toolset `nope`"),
        (r#"{"default": {"admission": "wait"}}"#, "unknown admission `wait`"),
        (r#"{"default": 3}"#, "`default` is an object"),
        (r#"{"default": {"confirm": ["tapp"]}}"#, "`default.confirm`: `tapp` is not a tool"),
        (r#"{"clients": {"ci": {"confirm": ["tap", "not_a_tool"]}}}"#, "`clients.ci.confirm`: `not_a_tool` is not a tool"),
    ] {
        let error = Policy::parse(text).expect_err(text);
        assert!(error.contains(says), "{text}: {error}");
    }
    assert!(Policy::from_file(Path::new("/nonexistent/policy.json")).expect_err("missing").contains("/nonexistent/policy.json"));
}

#[test]
fn a_client_toolset_narrows_what_the_server_serves_and_never_widens_it() {
    let served = Toolsets::parse("core").expect("core");
    let mut policy = ClientPolicy::default();
    assert!(policy.toolsets(&served).contains("tap") && !policy.toolsets(&served).contains("exec"));
    // The client's own set cuts the server's down to the tools in both, wherever each has them.
    policy.toolsets = Some(Toolsets::parse("input").expect("input"));
    let both = policy.toolsets(&served);
    assert!(both.contains("tap"), "in core and in input");
    assert!(!both.contains("boot"), "in core, not in input");
    assert!(!both.contains("touch"), "in input, not in core");
    // Naming a set the server doesn't serve gives nothing of it.
    policy.toolsets = Some(Toolsets::parse("debug").expect("debug"));
    assert!(!policy.toolsets(&served).contains("exec"));
    // All of them, within core.
    policy.toolsets = Some(Toolsets::all());
    assert!(policy.toolsets(&served).contains("tap") && !policy.toolsets(&served).contains("exec"));
    assert_eq!(json!(policy.toolsets(&served).limit().map(Toolsets::label)), json!("all"));
}
