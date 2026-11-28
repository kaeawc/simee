//! Device links (docs/design/device-links.md): `link`, `unlink`, `links` and `link_status`.
//!
//! iOS `watch-phone` links are CoreSimulator's own device pairs (#423), made, made active and
//! removed in-process through `sim-coresim` (`simctl pair`, `pair_activate`, `unpair` and `list
//! pairs` are the public front of the same calls). CoreSimulator's record (the device set's
//! `device_set.plist`) is the truth: a link's id is the pair's UUID, and every read re-derives the
//! state from it and from whether each member runs, so a server restart loses nothing. The other
//! kinds and Android's adapters (#424, #425, #426) come later and are refused with the reason.

use serde_json::{Value, json};
use sim_core::form_factor::FormFactor;
use sim_core::link::{self, LinkKind, LinkState, Member, Pair, Role};
use sim_core::platform::Platform;

use crate::methods::Session;
use crate::params::{opt_bool, str_param, strings};

pub const METHODS: &[&str] = &["link", "unlink", "links", "link_status"];

pub fn is_method(method: &str) -> bool {
    METHODS.contains(&method)
}

/// A simulator as a link sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sim {
    pub udid: String,
    pub name: String,
    pub form_factor: Option<FormFactor>,
    /// Booted.
    pub running: bool,
}

/// CoreSimulator's devices and device pairs in the call's device set: the iOS backend, or a fake.
pub(crate) trait PairHost {
    /// The simulator `query` (a UDID or a name) names; `None` when there is none.
    fn device(&mut self, query: &str) -> Result<Option<Sim>, String>;
    /// Every pair in the set.
    fn pairs(&mut self) -> Result<Vec<Pair>, String>;
    /// Whether CoreSimulator would pair them, without pairing: its refusal as the error.
    fn can_pair(&mut self, watch: &str, phone: &str) -> Result<(), String>;
    /// Pairs them: the new pair.
    fn pair(&mut self, watch: &str, phone: &str) -> Result<Pair, String>;
    /// Makes the pair its phone's active one.
    fn activate(&mut self, id: &str) -> Result<(), String>;
    /// Removes the pair.
    fn unpair(&mut self, id: &str) -> Result<(), String>;
}

/// The iOS backend on the call's device set (#409).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Backend<'a>(&'a mut crate::ios::Session);

#[cfg(target_os = "macos")]
impl PairHost for Backend<'_> {
    fn device(&mut self, query: &str) -> Result<Option<Sim>, String> {
        crate::ios::link_device(self.0, query)
    }
    fn pairs(&mut self) -> Result<Vec<Pair>, String> {
        crate::ios::link_pairs(self.0)
    }
    fn can_pair(&mut self, watch: &str, phone: &str) -> Result<(), String> {
        crate::ios::link_can_pair(self.0, watch, phone)
    }
    fn pair(&mut self, watch: &str, phone: &str) -> Result<Pair, String> {
        crate::ios::link_pair(self.0, watch, phone)
    }
    fn activate(&mut self, id: &str) -> Result<(), String> {
        crate::ios::link_activate(self.0, id)
    }
    fn unpair(&mut self, id: &str) -> Result<(), String> {
        crate::ios::link_unpair(self.0, id)
    }
}

/// No simulators, so no pairs, on a host without the simulator service.
#[cfg(not(target_os = "macos"))]
impl PairHost for Backend<'_> {
    fn device(&mut self, _: &str) -> Result<Option<Sim>, String> {
        Ok(None)
    }
    fn pairs(&mut self) -> Result<Vec<Pair>, String> {
        Ok(Vec::new())
    }
    fn can_pair(&mut self, _: &str, _: &str) -> Result<(), String> {
        Err(crate::ios::REASON.into())
    }
    fn pair(&mut self, _: &str, _: &str) -> Result<Pair, String> {
        Err(crate::ios::REASON.into())
    }
    fn activate(&mut self, _: &str) -> Result<(), String> {
        Err(crate::ios::REASON.into())
    }
    fn unpair(&mut self, _: &str) -> Result<(), String> {
        Err(crate::ios::REASON.into())
    }
}

/// Runs a link method: each device the call names is resolved across platforms first (an Android
/// one is refused until #445), then the method runs on the iOS backend.
pub(crate) fn call(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let mut p = p.clone();
    let mut devices = Vec::new();
    for q in strings(&p, "devices") {
        devices.push(Value::String(ios_device(s, method, &q, &p)?));
    }
    if !devices.is_empty() {
        p["devices"] = Value::Array(devices);
    }
    if let Some(q) = str_param(&p, "device").map(str::to_owned) {
        p["device"] = Value::String(ios_device(s, method, &q, &p)?);
    }
    run(&mut Backend(&mut s.ios), method, &p)
}

/// The id of the iOS device `query` names, or why it can't be linked.
fn ios_device(s: &mut Session, method: &str, query: &str, p: &Value) -> Result<String, String> {
    let (platform, id) = crate::methods::resolve(s, &json!({"device": query, "platform": p.get("platform")}))?;
    match platform {
        Platform::Ios => Ok(id),
        Platform::Android => Err(format!(
            "`{method}` links iOS devices for now: {query} is an Android device (Wear OS links are #445, Android Automotive #439)"
        )),
    }
}

/// A link method on `host`, past device resolution.
pub(crate) fn run(host: &mut dyn PairHost, method: &str, p: &Value) -> Result<Value, String> {
    match method {
        "link" => link(host, p),
        "unlink" => unlink(host, p),
        "links" => links(host, p),
        "link_status" => link_status(host, p),
        other => Err(format!("unknown link method `{other}`")),
    }
}

/// The simulator `query` names, or the error naming it.
fn need_device(host: &mut dyn PairHost, query: &str) -> Result<Sim, String> {
    host.device(query)?.ok_or_else(|| format!("no device matches `{query}`"))
}

fn opt_kind(p: &Value) -> Result<Option<LinkKind>, String> {
    str_param(p, "kind").map(LinkKind::parse).transpose()
}

/// The pair whose id is `id` (in any case), or the error naming it.
fn find_pair(host: &mut dyn PairHost, id: &str) -> Result<Pair, String> {
    host.pairs()?.into_iter().find(|p| p.id.eq_ignore_ascii_case(id)).ok_or_else(|| format!("no link `{id}` (`links` lists them)"))
}

/// A pair as the link object every method returns: its members (phone first), the state derived
/// from them, and whether it is its phone's active pair.
fn link_json(host: &mut dyn PairHost, pair: &Pair) -> Result<Value, String> {
    let phone = host.device(&pair.phone)?;
    let watch = host.device(&pair.watch)?;
    fn member<'a>(udid: &'a str, sim: &'a Option<Sim>) -> Member<'a> {
        match sim {
            Some(s) if s.running => Member::Running(&s.name),
            Some(s) => Member::Stopped(&s.name),
            None => Member::Missing(udid),
        }
    }
    let (state, reason) = link::pair_state(pair.connected, member(&pair.watch, &watch), member(&pair.phone, &phone));
    let json_member = |udid: &str, sim: &Option<Sim>, role: Role| {
        json!({
            "device": udid,
            "name": sim.as_ref().map(|s| s.name.as_str()),
            "platform": Platform::Ios.as_str(),
            "role": role.as_str(),
            "running": sim.as_ref().is_some_and(|s| s.running),
        })
    };
    Ok(json!({
        "id": pair.id,
        "kind": LinkKind::WatchPhone.as_str(),
        "mechanism": link::CORESIM_PAIR,
        "state": state.as_str(),
        "reason": reason,
        // CoreSimulator keeps no time for a pair, and simee keeps no copy of it yet.
        "since": null,
        "active": pair.active,
        "members": [json_member(&pair.phone, &phone, Role::Phone), json_member(&pair.watch, &watch, Role::Watch)],
    }))
}

/// `link`: pairs a watch with a phone, or returns the pair they already make; `link` (an id)
/// names a pair that exists instead. `active: true` makes it its phone's active one
/// (`pair_activate`). A pair this call made whose activation fails is unpaired again, so the
/// failed call leaves the set as it found it.
fn link(host: &mut dyn PairHost, p: &Value) -> Result<Value, String> {
    let activate = opt_bool(p, "active")? == Some(true);
    let (pair, made) = match str_param(p, "link") {
        Some(id) => {
            let pair = find_pair(host, id)?;
            for udid in [&pair.watch, &pair.phone] {
                if host.device(udid)?.is_none() {
                    return Err(format!("link {} is broken: {udid} no longer exists; `unlink` it", pair.id));
                }
            }
            (pair, false)
        }
        None => pair_devices(host, p)?,
    };
    if activate
        && !pair.active
        && let Err(e) = host.activate(&pair.id)
    {
        if !made {
            return Err(e);
        }
        return Err(match host.unpair(&pair.id) {
            Ok(()) => format!("activating the new link failed, so it was unpaired again (nothing was linked): {e}"),
            Err(u) => format!("activating the new link {} failed ({e}), and unpairing it again failed too ({u}): `unlink` it", pair.id),
        });
    }
    // Activating one pair makes the phone's others inactive: read it as CoreSimulator has it now.
    let pair = host.pairs()?.into_iter().find(|q| q.id == pair.id).unwrap_or(pair);
    Ok(json!({"link": link_json(host, &pair)?}))
}

/// The pair `devices` make, and whether this call made it: the one CoreSimulator already has, or a
/// new one, after the member rules and CoreSimulator's own check.
fn pair_devices(host: &mut dyn PairHost, p: &Value) -> Result<(Pair, bool), String> {
    let kind = opt_kind(p)?;
    let mut sims = Vec::new();
    for q in strings(p, "devices") {
        sims.push(need_device(host, &q)?);
    }
    let candidates: Vec<link::Candidate<'_>> =
        sims.iter().map(|s| link::Candidate { id: &s.udid, name: &s.name, platform: Platform::Ios, form_factor: s.form_factor }).collect();
    let (kind, roles) = link::plan(kind, &candidates)?;
    if kind != LinkKind::WatchPhone {
        return Err(format!(
            "{} links between iOS devices aren't implemented yet: only watch-phone, CoreSimulator's device pairs (#423); \
             headsets on the host network are #535, network links #427",
            kind.as_str()
        ));
    }
    let at = |role: Role| roles.iter().position(|r| *r == role).map(|i| &sims[i]).expect("plan gives a watch-phone link both roles");
    let (watch, phone) = (at(Role::Watch), at(Role::Phone));
    let pairs = host.pairs()?;
    match link::existing_pair(&watch.udid, &phone.udid, &pairs) {
        Ok(Some(same)) => return Ok((same.clone(), false)),
        Ok(None) => {}
        Err(_) => {
            // Neither the holder's name nor the link id: the caller may not be allowed to see that phone.
            return Err(format!("{} is already paired to another phone; `unlink` it first", watch.name));
        }
    }
    host.can_pair(&watch.udid, &phone.udid)?;
    Ok((host.pair(&watch.udid, &phone.udid)?, true))
}

/// `unlink`: removes the pair `link` names, or every pair of `device` (of `kind`). CoreSimulator
/// may shut the watch down (and erase it) as it unpairs: each teardown says whether the watch, if
/// it ran, was shut down.
fn unlink(host: &mut dyn PairHost, p: &Value) -> Result<Value, String> {
    let kind = opt_kind(p)?;
    let targets: Vec<Pair> = match (str_param(p, "link"), str_param(p, "device")) {
        (Some(id), _) => vec![find_pair(host, id)?],
        (None, Some(q)) => {
            let sim = need_device(host, q)?;
            if kind.is_some_and(|k| k != LinkKind::WatchPhone) {
                Vec::new()
            } else {
                host.pairs()?.into_iter().filter(|p| p.has(&sim.udid)).collect()
            }
        }
        (None, None) => return Err("give `link` (an id `links` lists) or `device` (every link of it)".into()),
    };
    let (mut unlinked, mut teardown, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    for pair in &targets {
        let ran = host.device(&pair.watch)?.is_some_and(|s| s.running);
        match host.unpair(&pair.id) {
            Ok(()) => {
                let runs = host.device(&pair.watch)?.is_some_and(|s| s.running);
                unlinked.push(json!(pair.id));
                teardown.push(json!({"id": pair.id, "watch": pair.watch, "shut_down": ran && !runs}));
            }
            Err(e) => {
                errors.push(format!("{}: {e}", pair.id));
                teardown.push(json!({"id": pair.id, "watch": pair.watch, "error": e}));
            }
        }
    }
    if unlinked.is_empty() && !errors.is_empty() {
        return Err(format!("unlink failed: {}", errors.join("; ")));
    }
    Ok(json!({"unlinked": unlinked, "teardown": teardown}))
}

/// `links`: every pair (of `device`, of `kind`, in `state`), the state derived now.
fn links(host: &mut dyn PairHost, p: &Value) -> Result<Value, String> {
    let kind = opt_kind(p)?;
    let state = str_param(p, "state").map(LinkState::parse).transpose()?;
    let device = match str_param(p, "device") {
        Some(q) => Some(need_device(host, q)?.udid),
        None => None,
    };
    if kind.is_some_and(|k| k != LinkKind::WatchPhone) {
        return Ok(json!([]));
    }
    let mut out = Vec::new();
    for pair in host.pairs()?.iter().filter(|p| device.as_deref().is_none_or(|d| p.has(d))) {
        let l = link_json(host, pair)?;
        if state.is_none_or(|s| l["state"] == s.as_str()) {
            out.push(l);
        }
    }
    Ok(Value::Array(out))
}

/// `link_status`: one link (by id, or the only one `device` is in) and what was checked.
fn link_status(host: &mut dyn PairHost, p: &Value) -> Result<Value, String> {
    let pair = match (str_param(p, "link"), str_param(p, "device")) {
        (Some(id), _) => find_pair(host, id)?,
        (None, Some(q)) => {
            let sim = need_device(host, q)?;
            let mut of: Vec<Pair> = host.pairs()?.into_iter().filter(|p| p.has(&sim.udid)).collect();
            match of.len() {
                0 => return Err(format!("{} is in no link", sim.name)),
                1 => of.remove(0),
                n => {
                    let ids: Vec<&str> = of.iter().map(|p| p.id.as_str()).collect();
                    return Err(format!("{} is in {n} links ({}); name one with `link`", sim.name, ids.join(", ")));
                }
            }
        }
        (None, None) => return Err("give `link` (an id `links` lists) or `device`".into()),
    };
    let watch = host.device(&pair.watch)?;
    let phone = host.device(&pair.phone)?;
    let check = |name: &str, ok: bool, detail: &str| json!({"name": name, "ok": ok, "detail": detail});
    let exists = |udid: &str, sim: &Option<Sim>| match sim {
        Some(s) => format!("{} ({udid})", s.name),
        None => format!("{udid} no longer exists"),
    };
    let running = |sim: &Option<Sim>| sim.as_ref().is_some_and(|s| s.running);
    let booted = |sim: &Option<Sim>| if running(sim) { "booted" } else { "shut down" };
    let checks = vec![
        check("watch exists", watch.is_some(), &exists(&pair.watch, &watch)),
        check("phone exists", phone.is_some(), &exists(&pair.phone, &phone)),
        check("watch running", running(&watch), booted(&watch)),
        check("phone running", running(&phone), booted(&phone)),
        check(
            "connected",
            pair.connected,
            if pair.connected { "CoreSimulator has the pair connected" } else { "CoreSimulator connects a pair once both are booted" },
        ),
        check(
            "active",
            pair.active,
            if pair.active {
                "the phone's active watch"
            } else {
                "another watch is the phone's active one: `link` with `active: true` makes this one"
            },
        ),
    ];
    Ok(json!({"link": link_json(host, &pair)?, "checks": checks}))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const PHONE: &str = "17000000-0000-4000-8000-000000000001";
    const PHONE2: &str = "17000000-0000-4000-8000-000000000002";
    const WATCH: &str = "A0000000-0000-4000-8000-000000000001";
    const WATCH2: &str = "A0000000-0000-4000-8000-000000000002";
    const IPAD: &str = "1BAD0000-0000-4000-8000-000000000001";

    /// A fake CoreSimulator device set: its simulators and pairs, as `device_set.plist` would hold
    /// them, and every change asked of it.
    #[derive(Default)]
    struct FakeSet {
        sims: BTreeMap<String, Sim>,
        pairs: Vec<Pair>,
        calls: Vec<String>,
        /// What `testDevicePairingBetweenGizmo:companion:error:` refuses with.
        refuse: Option<String>,
        /// Unpairing shuts the watch down (as CoreSimulator's log strings suggest it does).
        unpair_shuts_down: bool,
        /// What `setActiveDevicePair` fails with.
        refuse_activate: Option<String>,
        /// What unpairing fails with.
        refuse_unpair: Option<String>,
        next: u32,
    }

    impl FakeSet {
        fn with(sims: &[(&str, &str, FormFactor, bool)]) -> FakeSet {
            let mut set = FakeSet::default();
            for (udid, name, form_factor, running) in sims {
                set.sims.insert(
                    (*udid).into(),
                    Sim { udid: (*udid).into(), name: (*name).into(), form_factor: Some(*form_factor), running: *running },
                );
            }
            set
        }

        fn standard() -> FakeSet {
            FakeSet::with(&[
                (PHONE, "iPhone 17", FormFactor::Phone, true),
                (PHONE2, "iPhone 17 Pro", FormFactor::Phone, false),
                (WATCH, "Apple Watch Series 11", FormFactor::Watch, true),
                (WATCH2, "Apple Watch Ultra 3", FormFactor::Watch, false),
                (IPAD, "iPad Pro", FormFactor::Tablet, false),
            ])
        }

        fn changes(&self) -> Vec<&str> {
            self.calls.iter().map(String::as_str).filter(|c| !c.starts_with("check")).collect()
        }
    }

    impl PairHost for FakeSet {
        fn device(&mut self, query: &str) -> Result<Option<Sim>, String> {
            Ok(self.sims.values().find(|s| s.udid.eq_ignore_ascii_case(query) || s.name == query).cloned())
        }
        fn pairs(&mut self) -> Result<Vec<Pair>, String> {
            Ok(self.pairs.clone())
        }
        fn can_pair(&mut self, watch: &str, phone: &str) -> Result<(), String> {
            self.calls.push(format!("check {watch} {phone}"));
            self.refuse.clone().map_or(Ok(()), Err)
        }
        fn pair(&mut self, watch: &str, phone: &str) -> Result<Pair, String> {
            self.calls.push(format!("pair {watch} {phone}"));
            self.next += 1;
            // CoreSimulator makes a phone's first pair its active one.
            let active = !self.pairs.iter().any(|p| p.phone == phone);
            let both = self.sims[watch].running && self.sims[phone].running;
            let pair = Pair {
                id: format!("FA1A0000-0000-4000-8000-{:012}", self.next),
                watch: watch.into(),
                phone: phone.into(),
                active,
                connected: both,
            };
            self.pairs.push(pair.clone());
            Ok(pair)
        }
        fn activate(&mut self, id: &str) -> Result<(), String> {
            self.calls.push(format!("activate {id}"));
            if let Some(e) = &self.refuse_activate {
                return Err(e.clone());
            }
            let phone = self.pairs.iter().find(|p| p.id == id).map(|p| p.phone.clone()).ok_or("no such pair")?;
            for p in self.pairs.iter_mut().filter(|p| p.phone == phone) {
                p.active = p.id == id;
            }
            Ok(())
        }
        fn unpair(&mut self, id: &str) -> Result<(), String> {
            self.calls.push(format!("unpair {id}"));
            if let Some(e) = &self.refuse_unpair {
                return Err(e.clone());
            }
            let i = self.pairs.iter().position(|p| p.id == id).ok_or("no such pair")?;
            let pair = self.pairs.remove(i);
            if self.unpair_shuts_down {
                self.sims.get_mut(&pair.watch).expect("watch").running = false;
            }
            Ok(())
        }
    }

    fn link_of(set: &mut FakeSet, devices: &[&str], extra: Value) -> Result<Value, String> {
        let mut p = json!({"devices": devices});
        if let (Some(p), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
            p.extend(extra.clone());
        }
        run(set, "link", &p).map(|r| r["link"].clone())
    }

    #[test]
    fn link_pairs_a_watch_with_a_phone_and_returns_the_link() {
        let mut set = FakeSet::standard();
        let link = link_of(&mut set, &[WATCH, PHONE], json!({})).expect("paired");
        assert_eq!(set.changes(), [format!("pair {WATCH} {PHONE}")], "one pair, watch then phone");
        let id = set.pairs[0].id.clone();
        assert_eq!(link["id"], json!(id), "the link's id is CoreSimulator's pair UUID");
        assert_eq!(link["kind"], "watch-phone");
        assert_eq!(link["mechanism"], "coresim-pair");
        assert_eq!(link["state"], "active", "both run and CoreSimulator has it connected");
        assert_eq!(link["active"], true);
        assert_eq!(
            link["members"],
            json!([
                {"device": PHONE, "name": "iPhone 17", "platform": "ios", "role": "phone", "running": true},
                {"device": WATCH, "name": "Apple Watch Series 11", "platform": "ios", "role": "watch", "running": true},
            ])
        );
        // The phone first or a name: the same pair.
        let again = link_of(&mut set, &["iPhone 17", "Apple Watch Series 11"], json!({})).expect("again");
        assert_eq!(again, link, "linking the same pair again returns it");
        assert_eq!(set.changes().len(), 1, "and changes nothing more");
    }

    #[test]
    fn links_lists_every_pair_and_filters_by_device_kind_and_state() {
        let mut set = FakeSet::standard();
        link_of(&mut set, &[WATCH, PHONE], json!({})).expect("first");
        link_of(&mut set, &[WATCH2, PHONE], json!({})).expect("second watch on the same phone");
        let all = run(&mut set, "links", &json!({})).expect("links");
        assert_eq!(all.as_array().map(Vec::len), Some(2), "{all}");
        let second = &all.as_array().expect("array").iter().find(|l| l["members"][1]["device"] == WATCH2).expect("second").clone();
        assert_eq!(second["state"], "pending");
        assert_eq!(second["reason"], "Apple Watch Ultra 3 is shut down");
        assert_eq!(second["active"], false, "the phone's first watch stays its active one");
        let of_watch = run(&mut set, "links", &json!({"device": WATCH2})).expect("one device");
        assert_eq!(of_watch, json!([second]));
        assert_eq!(run(&mut set, "links", &json!({"device": PHONE})).expect("phone").as_array().map(Vec::len), Some(2));
        assert_eq!(run(&mut set, "links", &json!({"device": IPAD})).expect("none"), json!([]));
        assert_eq!(run(&mut set, "links", &json!({"state": "pending"})).expect("pending"), json!([second]));
        assert_eq!(run(&mut set, "links", &json!({"kind": "network"})).expect("no network links on iOS"), json!([]));
        assert_eq!(run(&mut set, "links", &json!({"kind": "watch-phone"})).expect("kind").as_array().map(Vec::len), Some(2));
        assert!(run(&mut set, "links", &json!({"state": "up"})).expect_err("bad state").contains("unknown link state"));
        let e = run(&mut set, "links", &json!({"device": "nothing"})).expect_err("no such device");
        assert!(e.contains("no device matches `nothing`"), "{e}");
    }

    #[test]
    fn active_true_makes_the_pair_its_phones_active_one() {
        let mut set = FakeSet::standard();
        link_of(&mut set, &[WATCH, PHONE], json!({})).expect("first");
        let second = link_of(&mut set, &[WATCH2, PHONE], json!({"active": true})).expect("second, active");
        assert_eq!(second["active"], true);
        let first = run(&mut set, "links", &json!({"device": WATCH})).expect("first");
        assert_eq!(first[0]["active"], false, "one active watch per phone");
        // Asking again for the active pair to be active activates nothing more.
        let calls = set.calls.len();
        link_of(&mut set, &[WATCH2, PHONE], json!({"active": true})).expect("again");
        assert_eq!(set.calls.len(), calls);
        // By id, as `pair_activate` takes it.
        let id = first[0]["id"].as_str().expect("id").to_owned();
        let back = run(&mut set, "link", &json!({"link": id, "active": true})).expect("by id");
        assert_eq!(back["link"]["active"], true);
        assert_eq!(set.changes().last().copied(), Some(format!("activate {id}").as_str()));
    }

    #[test]
    fn a_new_pair_whose_activation_fails_is_unpaired_again() {
        let mut set = FakeSet::standard();
        let first = link_of(&mut set, &[WATCH, PHONE], json!({})).expect("first")["id"].as_str().expect("id").to_owned();
        set.refuse_activate = Some("activate failed: the phone is busy".into());
        let e = link_of(&mut set, &[WATCH2, PHONE], json!({"active": true})).expect_err("activation fails");
        assert!(e.contains("unpaired again") && e.contains("the phone is busy"), "{e}");
        let made = "FA1A0000-0000-4000-8000-000000000002";
        assert_eq!(set.changes()[1..], [format!("pair {WATCH2} {PHONE}"), format!("activate {made}"), format!("unpair {made}")]);
        assert_eq!(set.pairs.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), [first.as_str()], "only the pair that was there");

        // A pair that was there before the call stays, whatever its activation does.
        set.pairs[0].active = false;
        let e = run(&mut set, "link", &json!({"link": first, "active": true})).expect_err("by id");
        assert_eq!(e, "activate failed: the phone is busy");
        assert_eq!(set.pairs.len(), 1);
        assert_eq!(set.changes().iter().filter(|c| c.starts_with("unpair")).count(), 1, "only the new pair was unpaired");

        // When unpairing fails too, the error names the pair left behind.
        set.refuse_unpair = Some("unpair failed".into());
        let e = link_of(&mut set, &[WATCH2, PHONE], json!({"active": true})).expect_err("both fail");
        let left = "FA1A0000-0000-4000-8000-000000000003";
        assert!(e.contains(left) && e.contains("unpair failed") && e.contains("`unlink` it"), "{e}");
        assert_eq!(set.pairs.len(), 2);
    }

    #[test]
    fn a_pair_of_the_wrong_form_factors_is_refused_before_anything_changes() {
        let mut set = FakeSet::standard();
        let e = link_of(&mut set, &[WATCH, WATCH2], json!({})).expect_err("two watches");
        assert!(e.contains("Apple Watch Series 11 is a watch and Apple Watch Ultra 3 is a watch"), "{e}");
        let e = link_of(&mut set, &[WATCH, IPAD], json!({"kind": "watch-phone"})).expect_err("an iPad");
        assert_eq!(e, "a watch-phone link pairs one watch with one phone: Apple Watch Series 11 is a watch and iPad Pro is a tablet");
        let e = link_of(&mut set, &[WATCH, IPAD], json!({})).expect_err("an iPad, kind inferred");
        assert!(e.contains("iPad Pro is a tablet"), "{e}");
        let e = link_of(&mut set, &[PHONE, PHONE2], json!({"kind": "network"})).expect_err("network on iOS");
        assert!(e.contains("network links") && e.contains("only watch-phone"), "{e}");
        assert!(link_of(&mut set, &[WATCH], json!({})).expect_err("one").contains("two or more"));
        assert!(link_of(&mut set, &[WATCH, "nothing"], json!({})).expect_err("missing").contains("no device matches `nothing`"));
        assert!(link_of(&mut set, &[WATCH, PHONE], json!({"kind": "pair"})).expect_err("bad kind").contains("unknown link kind"));
        assert!(set.calls.is_empty(), "nothing reached CoreSimulator: {:?}", set.calls);
    }

    #[test]
    fn a_watch_already_paired_with_another_phone_is_refused() {
        let mut set = FakeSet::standard();
        let first = link_of(&mut set, &[WATCH, PHONE], json!({})).expect("first");
        let e = link_of(&mut set, &[WATCH, PHONE2], json!({})).expect_err("already paired");
        let id = first["id"].as_str().expect("id");
        assert_eq!(e, "Apple Watch Series 11 is already paired to another phone; `unlink` it first");
        assert!(!e.contains("iPhone 17") && !e.contains(id), "names neither the phone nor the link: {e}");
        assert_eq!(set.changes().len(), 1, "only the first pair was made");
    }

    #[test]
    fn coresimulators_own_refusal_is_passed_through() {
        let mut set = FakeSet::standard();
        set.refuse = Some("pair failed: Failed to pair (pairs: 1 max: 1)".into());
        let e = link_of(&mut set, &[WATCH, PHONE], json!({})).expect_err("refused");
        assert!(e.contains("(pairs: 1 max: 1)"), "{e}");
        assert!(set.pairs.is_empty() && set.changes().is_empty());
    }

    #[test]
    fn unlink_removes_a_pair_by_id_or_every_pair_of_a_device() {
        let mut set = FakeSet::standard();
        set.unpair_shuts_down = true;
        let a = link_of(&mut set, &[WATCH, PHONE], json!({})).expect("a")["id"].as_str().expect("id").to_owned();
        let b = link_of(&mut set, &[WATCH2, PHONE], json!({})).expect("b")["id"].as_str().expect("id").to_owned();
        let r = run(&mut set, "unlink", &json!({"link": a})).expect("by id");
        assert_eq!(r["unlinked"], json!([a]));
        assert_eq!(r["teardown"], json!([{"id": a, "watch": WATCH, "shut_down": true}]), "the watch ran and CoreSimulator shut it down");
        assert_eq!(run(&mut set, "links", &json!({})).expect("links").as_array().map(Vec::len), Some(1));
        let e = run(&mut set, "unlink", &json!({"link": a})).expect_err("gone");
        assert!(e.contains(&format!("no link `{a}`")), "{e}");
        let r = run(&mut set, "unlink", &json!({"device": PHONE})).expect("every link of the phone");
        assert_eq!(r["unlinked"], json!([b]));
        assert_eq!(r["teardown"][0]["shut_down"], false, "the second watch wasn't running");
        assert_eq!(run(&mut set, "unlink", &json!({"device": PHONE})).expect("none left"), json!({"unlinked": [], "teardown": []}));
        assert_eq!(set.changes().iter().filter(|c| c.starts_with("unpair")).count(), 2);
        assert!(run(&mut set, "unlink", &json!({})).expect_err("nothing named").starts_with("give `link`"));
    }

    #[test]
    fn unlink_by_device_and_another_kind_leaves_the_pair() {
        let mut set = FakeSet::standard();
        link_of(&mut set, &[WATCH, PHONE], json!({})).expect("paired");
        assert_eq!(
            run(&mut set, "unlink", &json!({"device": WATCH, "kind": "network"})).expect("none"),
            json!({"unlinked": [], "teardown": []})
        );
        assert_eq!(set.pairs.len(), 1);
    }

    #[test]
    fn link_status_probes_one_link_with_its_checks() {
        let mut set = FakeSet::standard();
        let id = link_of(&mut set, &[WATCH2, PHONE], json!({})).expect("paired")["id"].as_str().expect("id").to_owned();
        let r = run(&mut set, "link_status", &json!({"device": WATCH2})).expect("status");
        assert_eq!(r["link"]["id"], json!(id));
        let checks: Vec<(String, bool)> = r["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .map(|c| (c["name"].as_str().expect("name").to_owned(), c["ok"].as_bool().expect("ok")))
            .collect();
        let want = [
            ("watch exists", true),
            ("phone exists", true),
            ("watch running", false),
            ("phone running", true),
            ("connected", false),
            ("active", true),
        ];
        assert_eq!(checks, want.map(|(n, ok)| (n.to_owned(), ok)));
        assert_eq!(run(&mut set, "link_status", &json!({"link": id})).expect("by id")["link"]["id"], json!(id));
        link_of(&mut set, &[WATCH, PHONE], json!({})).expect("a second watch");
        let e = run(&mut set, "link_status", &json!({"device": PHONE})).expect_err("two links");
        assert!(e.contains("iPhone 17 is in 2 links") && e.contains("name one with `link`"), "{e}");
        assert!(run(&mut set, "link_status", &json!({"device": IPAD})).expect_err("none").contains("iPad Pro is in no link"));
    }

    #[test]
    fn a_pair_whose_member_is_gone_is_broken() {
        let mut set = FakeSet::standard();
        link_of(&mut set, &[WATCH, PHONE], json!({})).expect("paired");
        set.sims.remove(WATCH);
        let l = run(&mut set, "links", &json!({})).expect("links");
        assert_eq!(l[0]["state"], "broken");
        assert_eq!(l[0]["reason"], format!("{WATCH} no longer exists"));
        assert_eq!(l[0]["members"][1], json!({"device": WATCH, "name": null, "platform": "ios", "role": "watch", "running": false}));
    }

    #[test]
    fn the_docs_and_schemas_agree_with_the_method_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(api) = std::fs::read_to_string(root.join("docs/api.md")) else {
            eprintln!("skipped: not in the simee workspace, so docs/api.md isn't here");
            return;
        };
        for (method, params, doc) in crate::METHODS.iter().filter(|(m, ..)| is_method(m)) {
            let platforms: Vec<&str> = crate::platforms(method).iter().map(|p| p.label()).collect();
            let row = format!("| `{method}` | {} | {} | {} |", platforms.join(", "), params.replace('|', "\\|"), doc.replace('|', "\\|"));
            assert!(api.contains(&row), "docs/api.md's row for `{method}` differs from METHODS; it should be\n{row}");
            assert!(params.contains("set?"), "{method} takes the device set (#409)");
            assert!(crate::output_schema(method).is_some(), "{method} has no output schema");
            let schema = crate::input_schema(method).expect("input schema");
            for (name, prop) in schema["properties"].as_object().expect("properties") {
                assert!(prop.get("type").is_some(), "{method}'s `{name}` has no type in schema.rs");
            }
        }
        let schema = crate::input_schema("links").expect("links");
        assert_eq!(schema["properties"]["state"]["type"], "string", "a link state's name, not a number");
        assert_eq!(crate::input_schema("link").expect("link")["properties"]["active"]["type"], "boolean");
    }

    #[test]
    fn the_methods_are_lifecycle_tools_with_their_effects() {
        use crate::catalog::{Effect, Group, effect, group};
        for m in METHODS {
            assert_eq!(group(m), Some(Group::Lifecycle), "{m}");
            assert_eq!(crate::platforms(m), [Platform::Ios], "{m}: iOS until the Android adapters land");
        }
        assert_eq!(effect("links"), Some(Effect::Read));
        assert_eq!(effect("link_status"), Some(Effect::Read));
        assert_eq!(effect("link"), Some(Effect::Set));
        // CoreSimulator's unpair may shut the watch down and erase it (docs/design/device-links.md,
        // open question 12): classed as what it may do until a device shows it doesn't.
        assert_eq!(effect("unlink"), Some(Effect::Destroy));
        // Not in `core` yet: the design puts `links` and `link_status` there, but their schemas
        // grow the default tools/list past the MCP evals' 10% budget (+21.7%).
        assert!(METHODS.iter().all(|m| !crate::catalog::CORE.contains(m)));
    }
}
