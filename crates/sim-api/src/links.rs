//! Device links (docs/design/device-links.md): `link`, `unlink`, `links` and `link_status`.
//!
//! iOS `watch-phone` links are CoreSimulator's own device pairs (#423), made, made active and
//! removed in-process through `sim-coresim` (`simctl pair`, `pair_activate`, `unpair` and `list
//! pairs` are the public front of the same calls). CoreSimulator's record (the device set's
//! `device_set.plist`) is the truth: a link's id is the pair's UUID, and every read re-derives the
//! state from it and from whether each member runs, so a server restart loses nothing.
//!
//! `headset-phone` links, on both platforms, are records simee keeps in each member's
//! `links.json` (`records.rs`, #535). Wear OS, Android Automotive and network links (#445, #439,
//! #427) come later and are refused with the reason.

pub(crate) mod records;

use serde_json::{Value, json};
use sim_core::form_factor::FormFactor;
use sim_core::link::{self, LinkKind, LinkState, Member, Pair, Record, Role};
use sim_core::platform::Platform;

use self::records::{Dev, RecordHost};
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

/// Runs a link method: each device the call names is resolved across platforms first, then the
/// method runs on CoreSimulator's pairs, simee's records or both ([`dispatch`]).
pub(crate) fn call(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let mut p = p.clone();
    let mut devices = Vec::new();
    for q in strings(&p, "devices") {
        devices.push(resolve(s, &q, &p)?);
    }
    if !devices.is_empty() {
        p["devices"] = devices.iter().map(|(_, id)| Value::String(id.clone())).collect();
    }
    let device = match str_param(&p, "device").map(str::to_owned) {
        Some(q) => Some(resolve(s, &q, &p)?),
        None => None,
    };
    if let Some((_, id)) = &device {
        p["device"] = Value::String(id.clone());
    }
    dispatch(&mut Hosts(s), method, &p, &devices, device.as_ref())
}

/// The platform and id of the device `query` names.
fn resolve(s: &mut Session, query: &str, p: &Value) -> Result<(Platform, String), String> {
    crate::methods::resolve(s, &json!({"device": query, "platform": p.get("platform")}))
}

/// Both backends on the call's session: CoreSimulator's pairs ([`PairHost`]) and the devices,
/// files and forwards simee's records need ([`RecordHost`]).
struct Hosts<'a>(&'a mut Session);

impl PairHost for Hosts<'_> {
    fn device(&mut self, query: &str) -> Result<Option<Sim>, String> {
        Backend(&mut self.0.ios).device(query)
    }
    fn pairs(&mut self) -> Result<Vec<Pair>, String> {
        Backend(&mut self.0.ios).pairs()
    }
    fn can_pair(&mut self, watch: &str, phone: &str) -> Result<(), String> {
        Backend(&mut self.0.ios).can_pair(watch, phone)
    }
    fn pair(&mut self, watch: &str, phone: &str) -> Result<Pair, String> {
        Backend(&mut self.0.ios).pair(watch, phone)
    }
    fn activate(&mut self, id: &str) -> Result<(), String> {
        Backend(&mut self.0.ios).activate(id)
    }
    fn unpair(&mut self, id: &str) -> Result<(), String> {
        Backend(&mut self.0.ios).unpair(id)
    }
}

/// An AVD as a record sees it.
fn android_dev(d: &sim_android::Device) -> Dev {
    Dev {
        platform: Platform::Android,
        id: d.info.udid.clone(),
        name: d.info.name.clone(),
        form_factor: d.info.form_factor,
        glasses: d.is_glasses(),
        running: d.running.is_some(),
    }
}

/// A simulator as a record sees it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn ios_dev(s: Sim) -> Dev {
    Dev { platform: Platform::Ios, id: s.udid, name: s.name, form_factor: s.form_factor, glasses: false, running: s.running }
}

impl Hosts<'_> {
    /// simee's state directory of `dev`: `<avd>/simee/` or `<data>/simee/`.
    fn state_dir(&mut self, dev: &Dev) -> Result<std::path::PathBuf, String> {
        match dev.platform {
            Platform::Android => {
                self.0.android.backend().device(&dev.id)?.state_dir().ok_or_else(|| format!("{}'s config can't be read", dev.name))
            }
            #[cfg(target_os = "macos")]
            Platform::Ios => crate::ios::link_state_dir(&mut self.0.ios, &dev.id),
            #[cfg(not(target_os = "macos"))]
            Platform::Ios => Err(crate::ios::REASON.into()),
        }
    }

    /// adb bound to the running Android `dev`.
    fn adb(&mut self, dev: &Dev) -> Result<sim_android::apps::Adb, String> {
        if dev.platform == Platform::Ios {
            return Err("iOS simulators share the host's network: nothing is forwarded".into());
        }
        let android = self.0.android.backend();
        android.device(&dev.id)?.adb(android)
    }

    fn read_file(&mut self, dev: &Dev, dir: &std::path::Path) -> Result<Vec<Record>, String> {
        let path = dir.join(records::LINKS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let (links, warnings) = records::parse_file(&text, (dev.platform, &dev.id), &path.display().to_string());
        for w in warnings {
            eprintln!("simee: {w}");
        }
        Ok(links)
    }
}

impl RecordHost for Hosts<'_> {
    fn device(&mut self, platform: Platform, id: &str) -> Result<Option<Dev>, String> {
        match platform {
            Platform::Android => {
                let android = self.0.android.backend();
                if !android.has_avd(id) {
                    return Ok(None);
                }
                Ok(Some(android_dev(&android.device(id)?)))
            }
            #[cfg(target_os = "macos")]
            Platform::Ios => Ok(crate::ios::link_device(&mut self.0.ios, id)?.map(ios_dev)),
            #[cfg(not(target_os = "macos"))]
            Platform::Ios => Ok(None),
        }
    }
    fn all(&mut self) -> Result<Vec<Dev>, String> {
        let android = self.0.android.backend();
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut out: Vec<Dev> = android.devices().iter().filter_map(|i| android.device(&i.udid).ok()).map(|d| android_dev(&d)).collect();
        #[cfg(target_os = "macos")]
        out.extend(crate::ios::link_devices(&mut self.0.ios)?.into_iter().map(ios_dev));
        Ok(out)
    }
    fn read(&mut self, dev: &Dev) -> Result<Vec<Record>, String> {
        let dir = self.state_dir(dev)?;
        self.read_file(dev, &dir)
    }
    fn update(&mut self, dev: &Dev, change: &mut dyn FnMut(&mut Vec<Record>)) -> Result<(), String> {
        let dir = self.state_dir(dev)?;
        let _lock = sim_android::StateLock::take(&dir, records::LINKS_FILE)?;
        let mut links = self.read_file(dev, &dir)?;
        change(&mut links);
        sim_android::write_state_file(&dir, records::LINKS_FILE, records::file_text(&links).as_deref())
    }
    fn hold(&mut self, dev: &Dev) -> Result<Box<dyn std::any::Any>, String> {
        let dir = self.state_dir(dev)?;
        Ok(Box::new(sim_android::StateLock::take(&dir, records::LINK_LOCK)?))
    }
    fn forward(&mut self, dev: &Dev, port: u16) -> Result<u16, String> {
        self.adb(dev)?.forward(port)
    }
    fn remove_forward(&mut self, dev: &Dev, host_port: u16) -> Result<(), String> {
        self.adb(dev)?.remove_forward(host_port)
    }
    fn forwards(&mut self, dev: &Dev) -> Result<Vec<(u16, u16)>, String> {
        self.adb(dev)?.forwards()
    }
    fn listening(&mut self, dev: &Dev) -> Result<Vec<u16>, String> {
        self.adb(dev)?.listening()
    }
    fn new_id(&mut self) -> Result<String, String> {
        sim_core::entropy::hex(4).map(|h| format!("lnk-{h}")).map_err(|e| e.to_string())
    }
    fn now(&mut self) -> String {
        crate::prefs::Date::from_unix(crate::prefs::Date::now().unix_secs(), 0).to_iso8601()
    }
}

/// A link method past device resolution (`devices` and `device` as (platform, id)): a watch pair
/// goes to CoreSimulator ([`run`]), a kept link (`lnk-…`, `headset-phone`) to its records, and a
/// listing, an unlink or a status by device to both.
pub(crate) fn dispatch<H: PairHost + RecordHost>(
    h: &mut H,
    method: &str,
    p: &Value,
    devices: &[(Platform, String)],
    device: Option<&(Platform, String)>,
) -> Result<Value, String> {
    match method {
        "link" => link_any(h, p, devices),
        "unlink" => unlink_any(h, p, device),
        "links" => links_any(h, p, device),
        "link_status" => status_any(h, p, device),
        other => Err(format!("unknown link method `{other}`")),
    }
}

/// The record device `(platform, id)`, or the error naming it.
fn need_dev(h: &mut dyn RecordHost, (platform, id): &(Platform, String)) -> Result<Dev, String> {
    h.device(*platform, id)?.ok_or_else(|| format!("no device matches `{id}`"))
}

fn link_any<H: PairHost + RecordHost>(h: &mut H, p: &Value, devices: &[(Platform, String)]) -> Result<Value, String> {
    if let Some(id) = str_param(p, "link") {
        if !records::is_record_id(id) {
            return run(h, "link", p);
        }
        let r = records::find(h, id)?;
        let devs = records::existing_members(h, &r)?;
        return link_devs(h, Some(LinkKind::parse(&r.kind)?), &devs, p);
    }
    let mut devs = Vec::new();
    for d in devices {
        devs.push(need_dev(h, d)?);
    }
    link_devs(h, records::opt_kind(p)?, &devs, p)
}

/// The member rules on `devs`, then the adapter their kind and platform take.
fn link_devs<H: PairHost + RecordHost>(h: &mut H, kind: Option<LinkKind>, devs: &[Dev], p: &Value) -> Result<Value, String> {
    let candidates: Vec<link::Candidate<'_>> =
        devs.iter().map(|d| link::Candidate { id: &d.id, name: &d.name, platform: d.platform, form_factor: d.form_factor }).collect();
    let (kind, roles) = link::plan(kind, &candidates)?;
    match (kind, devs[0].platform) {
        (LinkKind::HeadsetPhone, _) => records::link(h, kind, devs, &roles, p),
        // CoreSimulator's pairs, and the refusals of the kinds iOS doesn't have yet.
        (_, Platform::Ios) => run(h, "link", p),
        (kind, Platform::Android) => Err(format!(
            "{} links between Android devices aren't implemented yet: only headset-phone (an Android XR headset and a phone, \
             net-forward); Wear OS links are #445, Android Automotive #439, network links #427",
            kind.as_str()
        )),
    }
}

fn unlink_any<H: PairHost + RecordHost>(h: &mut H, p: &Value, device: Option<&(Platform, String)>) -> Result<Value, String> {
    let kind = records::opt_kind(p)?;
    match (str_param(p, "link"), device) {
        (Some(id), _) if records::is_record_id(id) => {
            let r = records::find(h, id)?;
            let (unlinked, teardown) = records::unlink(h, &[r])?;
            Ok(json!({"unlinked": unlinked, "teardown": teardown}))
        }
        (None, Some(d)) => {
            let mut out = if d.0 == Platform::Ios { run(h, "unlink", p)? } else { json!({"unlinked": [], "teardown": []}) };
            let Some(dev) = RecordHost::device(h, d.0, &d.1)? else {
                return if d.0 == Platform::Ios { Ok(out) } else { Err(format!("no device matches `{}`", d.1)) };
            };
            let targets: Vec<Record> =
                records::of_device(h, &dev)?.into_iter().filter(|r| kind.is_none_or(|k| r.kind == k.as_str())).collect();
            let (unlinked, teardown) = records::unlink(h, &targets)?;
            for (key, more) in [("unlinked", unlinked), ("teardown", teardown)] {
                if let Some(list) = out[key].as_array_mut() {
                    list.extend(more);
                }
            }
            Ok(out)
        }
        _ => run(h, "unlink", p),
    }
}

fn links_any<H: PairHost + RecordHost>(h: &mut H, p: &Value, device: Option<&(Platform, String)>) -> Result<Value, String> {
    let kind = records::opt_kind(p)?;
    let state = str_param(p, "state").map(LinkState::parse).transpose()?;
    let mut out = match device {
        Some((Platform::Android, _)) => Vec::new(),
        _ => run(h, "links", p)?.as_array().cloned().unwrap_or_default(),
    };
    let dev = match device {
        Some(d) => Some(need_dev(h, d)?),
        None => None,
    };
    out.extend(records::links(h, dev.as_ref(), kind, state)?);
    Ok(Value::Array(out))
}

fn status_any<H: PairHost + RecordHost>(h: &mut H, p: &Value, device: Option<&(Platform, String)>) -> Result<Value, String> {
    let d = match (str_param(p, "link"), device) {
        (Some(id), _) if records::is_record_id(id) => {
            let r = records::find(h, id)?;
            return records::status(h, &r);
        }
        (None, Some(d)) => d,
        _ => return run(h, "link_status", p),
    };
    let dev = RecordHost::device(h, d.0, &d.1)?;
    let recs = match &dev {
        Some(dev) => records::of_device(h, dev)?,
        None => Vec::new(),
    };
    let pairs: Vec<Pair> =
        if d.0 == Platform::Ios { PairHost::pairs(h)?.into_iter().filter(|q| q.has(&d.1)).collect() } else { Vec::new() };
    match (recs.as_slice(), pairs.len()) {
        ([r], 0) => records::status(h, r),
        ([], 1) => run(h, "link_status", p),
        ([], 0) => match dev {
            Some(dev) => Err(format!("{} is in no link", dev.name)),
            None => run(h, "link_status", p),
        },
        _ => {
            let ids: Vec<&str> = recs.iter().map(|r| r.id.as_str()).chain(pairs.iter().map(|q| q.id.as_str())).collect();
            let name = dev.as_ref().map_or(d.1.as_str(), |dev| dev.name.as_str());
            Err(format!("{name} is in {} links ({}); name one with `link`", ids.len(), ids.join(", ")))
        }
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
            "{} links between iOS devices aren't implemented yet: watch-phone (CoreSimulator's device pairs, #423) and \
             headset-phone (on the host's network) are; network links are #427",
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
        assert!(e.contains("network links are #427") && e.contains("watch-phone"), "{e}");
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

    const VISION: &str = "B0000000-0000-4000-8000-000000000001";

    /// CoreSimulator's set and simee's records on one host, as `call` sees them: the iOS
    /// simulators are in both (the record host sees what CoreSimulator lists), the AVDs only in
    /// the records.
    struct Both {
        set: FakeSet,
        rec: records::fake::FakeRecords,
    }

    impl PairHost for Both {
        fn device(&mut self, query: &str) -> Result<Option<Sim>, String> {
            self.set.device(query)
        }
        fn pairs(&mut self) -> Result<Vec<Pair>, String> {
            self.set.pairs()
        }
        fn can_pair(&mut self, watch: &str, phone: &str) -> Result<(), String> {
            self.set.can_pair(watch, phone)
        }
        fn pair(&mut self, watch: &str, phone: &str) -> Result<Pair, String> {
            self.set.pair(watch, phone)
        }
        fn activate(&mut self, id: &str) -> Result<(), String> {
            self.set.activate(id)
        }
        fn unpair(&mut self, id: &str) -> Result<(), String> {
            self.set.unpair(id)
        }
    }

    impl RecordHost for Both {
        fn device(&mut self, platform: Platform, id: &str) -> Result<Option<Dev>, String> {
            if platform == Platform::Ios {
                return Ok(self.set.device(id)?.map(ios_dev));
            }
            self.rec.device(platform, id)
        }
        fn all(&mut self) -> Result<Vec<Dev>, String> {
            let mut all = self.rec.all()?;
            all.extend(self.set.sims.values().cloned().map(ios_dev));
            Ok(all)
        }
        fn read(&mut self, dev: &Dev) -> Result<Vec<Record>, String> {
            self.rec.read(dev)
        }
        fn update(&mut self, dev: &Dev, change: &mut dyn FnMut(&mut Vec<Record>)) -> Result<(), String> {
            self.rec.update(dev, change)
        }
        fn hold(&mut self, dev: &Dev) -> Result<Box<dyn std::any::Any>, String> {
            self.rec.hold(dev)
        }
        fn forward(&mut self, dev: &Dev, port: u16) -> Result<u16, String> {
            self.rec.forward(dev, port)
        }
        fn remove_forward(&mut self, dev: &Dev, host_port: u16) -> Result<(), String> {
            self.rec.remove_forward(dev, host_port)
        }
        fn forwards(&mut self, dev: &Dev) -> Result<Vec<(u16, u16)>, String> {
            self.rec.forwards(dev)
        }
        fn listening(&mut self, dev: &Dev) -> Result<Vec<u16>, String> {
            self.rec.listening(dev)
        }
        fn new_id(&mut self) -> Result<String, String> {
            self.rec.new_id()
        }
        fn now(&mut self) -> String {
            self.rec.now()
        }
    }

    /// The standard set with a Vision Pro, and AVDs: a phone, an Android XR headset, a pair of
    /// glasses and a Wear OS watch, all running but the second phone.
    fn both() -> Both {
        let mut set = FakeSet::standard();
        set.sims.insert(
            VISION.into(),
            Sim { udid: VISION.into(), name: "Apple Vision Pro".into(), form_factor: Some(FormFactor::Vision), running: true },
        );
        let mut rec = records::fake::FakeRecords::with(&[
            (Platform::Android, "pixel", FormFactor::Phone, true),
            (Platform::Android, "pixel2", FormFactor::Phone, false),
            (Platform::Android, "xr", FormFactor::Vision, true),
            (Platform::Android, "glasses", FormFactor::Vision, true),
            (Platform::Android, "wear", FormFactor::Watch, true),
        ]);
        rec.dev_mut("glasses").glasses = true;
        Both { set, rec }
    }

    /// What `call` does past `methods::resolve`: an AVD by name, else a simulator by UDID or name.
    fn on(h: &mut Both, method: &str, p: Value) -> Result<Value, String> {
        let lookup = |h: &mut Both, q: &str| -> (Platform, String) {
            if let Some(d) = h.rec.devs.iter().find(|d| d.id.eq_ignore_ascii_case(q)) {
                return (Platform::Android, d.id.clone());
            }
            let udid = h.set.sims.values().find(|s| s.name == q).map_or_else(|| q.to_owned(), |s| s.udid.clone());
            (Platform::Ios, udid)
        };
        let devices: Vec<(Platform, String)> = strings(&p, "devices").iter().map(|q| lookup(h, q)).collect();
        let device = str_param(&p, "device").map(|q| lookup(h, q));
        let mut p = p;
        if !devices.is_empty() {
            p["devices"] = devices.iter().map(|(_, id)| json!(id)).collect();
        }
        if let Some((_, id)) = &device {
            p["device"] = json!(id);
        }
        dispatch(h, method, &p, &devices, device.as_ref())
    }

    fn check_list(status: &Value) -> Vec<(String, bool)> {
        status["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .map(|c| (c["name"].as_str().expect("name").to_owned(), c["ok"].as_bool().expect("ok")))
            .collect()
    }

    #[test]
    fn an_android_xr_headset_and_a_phone_link_with_net_forward() {
        let mut h = both();
        let link = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect("linked")["link"].clone();
        assert_eq!(link["id"], "lnk-00000001");
        assert_eq!(link["kind"], "headset-phone", "inferred from vision + phone");
        assert_eq!(link["mechanism"], "net-forward");
        assert_eq!(link["state"], "active");
        assert_eq!(link["active"], Value::Null, "only a watch pair has an active one");
        assert_eq!(link["since"], "2026-11-28T12:00:00Z");
        assert_eq!(
            link["members"],
            json!([
                {"device": "pixel", "name": "pixel", "platform": "android", "role": "phone", "running": true},
                {"device": "xr", "name": "xr", "platform": "android", "role": "headset", "running": true},
            ])
        );
        assert_eq!(link["forwards"], json!([{"device": "xr", "port": 8080, "host_port": 41001}]));
        assert_eq!(h.rec.changes(), ["forward xr 8080 -> 41001"]);
        for member in ["pixel", "xr"] {
            let copy = h.rec.copy(Platform::Android, member);
            assert_eq!(copy.len(), 1, "{member} keeps a copy");
            assert_eq!((copy[0].id.as_str(), copy[0].rev), ("lnk-00000001", 1));
        }
        assert!(h.set.calls.is_empty(), "nothing reached CoreSimulator");

        // The same devices again: the same link, its forward kept, nothing written.
        let calls = h.rec.calls.len();
        let again = on(&mut h, "link", json!({"devices": ["pixel", "xr"], "kind": "headset-phone"})).expect("again")["link"].clone();
        assert_eq!(again, link);
        assert_eq!(h.rec.calls.len(), calls, "linking again changes nothing more");
        assert_eq!(on(&mut h, "links", json!({})).expect("links"), json!([link]));
        assert_eq!(on(&mut h, "links", json!({"device": "xr"})).expect("of xr"), json!([link]));
        assert_eq!(on(&mut h, "links", json!({"device": "pixel2"})).expect("none"), json!([]));
        assert_eq!(on(&mut h, "links", json!({"kind": "watch-phone"})).expect("kind"), json!([]));
    }

    #[test]
    fn link_status_says_which_ports_are_forwarded_and_listen() {
        let mut h = both();
        on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080], "pixel": [9000]}})).expect("linked");
        h.rec.listening.insert("xr".into(), vec![8080]);
        let status = on(&mut h, "link_status", json!({"device": "xr"})).expect("status");
        let want = [
            ("phone exists", true),
            ("headset exists", true),
            ("phone running", true),
            ("headset running", true),
            // `ports` in the order its keys sort.
            ("forward pixel:9000", true),
            ("pixel listens on 9000", false),
            ("forward xr:8080", true),
            ("xr listens on 8080", true),
        ];
        assert_eq!(check_list(&status), want.map(|(n, ok)| (n.to_owned(), ok)));
        assert_eq!(
            status["checks"][6]["detail"],
            "127.0.0.1:41002 on the host, 10.0.2.2:41002 from the other device, reaches xr's port 8080"
        );
        assert_eq!(status["link"]["state"], "pending", "a forwarded port nothing listens on yet");
        assert_eq!(status["link"]["reason"], "nothing in pixel listens on 9000 yet");

        h.rec.listening.insert("pixel".into(), vec![9000]);
        let status = on(&mut h, "link_status", json!({"link": "lnk-00000001"})).expect("by id");
        assert_eq!(status["link"]["state"], "active");

        // An adb server restart drops the forwards: the probe sees it, the files don't.
        h.rec.forwards.clear();
        let status = on(&mut h, "link_status", json!({"device": "pixel"})).expect("status");
        assert_eq!(status["link"]["state"], "pending");
        assert!(status["link"]["reason"].as_str().expect("reason").contains("tcp:41001 to pixel's port 9000 is gone"), "{status}");
        assert_eq!(on(&mut h, "links", json!({})).expect("links")[0]["state"], "active", "`links` reads the files only");
        // Linking again forwards them anew.
        let again = on(&mut h, "link", json!({"devices": ["xr", "pixel"]})).expect("again")["link"].clone();
        assert_eq!(
            again["forwards"],
            json!([{"device": "pixel", "port": 9000, "host_port": 41003}, {"device": "xr", "port": 8080, "host_port": 41004}])
        );
        assert_eq!(h.rec.copy(Platform::Android, "xr")[0].rev, 2);
        assert_eq!(on(&mut h, "link_status", json!({"device": "xr"})).expect("status")["link"]["state"], "active");
    }

    #[test]
    fn a_shut_down_member_leaves_its_port_unforwarded_until_linked_again() {
        let mut h = both();
        h.rec.dev_mut("xr").running = false;
        let link = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": 8080}})).expect("linked")["link"].clone();
        assert_eq!((link["state"].as_str(), link["reason"].as_str()), (Some("pending"), Some("xr is shut down")));
        assert_eq!(link["forwards"], json!([{"device": "xr", "port": 8080, "host_port": null}]));
        assert!(h.rec.changes().is_empty(), "no forward on a device that isn't running");
        h.rec.dev_mut("xr").running = true;
        let listed = on(&mut h, "links", json!({})).expect("links");
        assert_eq!(listed[0]["state"], "pending");
        assert!(listed[0]["reason"].as_str().expect("reason").starts_with("xr's port 8080 isn't forwarded"), "{listed}");
        let status = on(&mut h, "link_status", json!({"device": "xr"})).expect("status");
        assert_eq!(check_list(&status)[4], ("forward xr:8080".to_owned(), false));
        let link = on(&mut h, "link", json!({"devices": ["xr", "pixel"]})).expect("again")["link"].clone();
        assert_eq!((link["state"].as_str(), &link["forwards"][0]["host_port"]), (Some("active"), &json!(41001)));
        // Ports the call names replace the ones the link had.
        let link =
            on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"pixel": [5000]}})).expect("other ports")["link"].clone();
        assert_eq!(link["forwards"], json!([{"device": "pixel", "port": 5000, "host_port": 41002}]));
        assert_eq!(h.rec.changes().last().copied(), Some("remove xr 41001"));
    }

    #[test]
    fn glasses_are_refused_until_the_bluetooth_bridge() {
        let mut h = both();
        let e = on(&mut h, "link", json!({"devices": ["glasses", "pixel"]})).expect_err("glasses");
        assert!(e.starts_with("glasses is a pair of Android XR glasses") && e.contains("#435"), "{e}");
        let e = on(&mut h, "link", json!({"devices": ["pixel", "glasses"], "kind": "headset-phone"})).expect_err("glasses");
        assert!(e.contains("#435"), "{e}");
        assert!(h.rec.calls.is_empty() && h.rec.files.is_empty(), "nothing changed");
    }

    #[test]
    fn other_android_kinds_and_wrong_members_are_refused_before_anything_changes() {
        let mut h = both();
        let e = on(&mut h, "link", json!({"devices": ["wear", "pixel"]})).expect_err("wear os");
        assert!(e.starts_with("watch-phone links between Android devices aren't implemented yet") && e.contains("#445"), "{e}");
        let e = on(&mut h, "link", json!({"devices": ["xr", "wear"], "kind": "headset-phone"})).expect_err("a watch");
        assert_eq!(e, "a headset-phone link pairs one headset with one phone: xr is a vision and wear is a watch");
        let e = on(&mut h, "link", json!({"devices": ["xr", PHONE]})).expect_err("cross-platform");
        assert!(e.starts_with("only network links cross platforms"), "{e}");
        let e = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "active": true})).expect_err("active");
        assert!(e.contains("a headset-phone link has none"), "{e}");
        let e = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"wear": [1]}})).expect_err("ports of another");
        assert!(e.contains("`ports` names `wear`"), "{e}");
        h.rec.refuse_forward = Some("adb: error: cannot bind listener".into());
        let e = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect_err("forward fails");
        assert_eq!(e, "forwarding xr's port 8080: adb: error: cannot bind listener");
        assert!(h.rec.files.is_empty() && h.rec.changes().is_empty() && h.set.calls.is_empty());
    }

    #[test]
    fn a_copy_that_cant_be_written_is_written_by_the_next_link_and_one_written_nowhere_takes_its_forwards_back() {
        let mut h = both();
        h.rec.refuse_update = vec!["pixel".into()];
        let e = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect_err("pixel's copy");
        assert!(e.contains("was saved with xr but not with pixel: read-only file system"), "{e}");
        assert_eq!(h.rec.forwards.len(), 1, "the link exists in xr's copy and holds its forward");
        h.rec.refuse_update.clear();
        let link = on(&mut h, "link", json!({"devices": ["xr", "pixel"]})).expect("again")["link"].clone();
        assert_eq!(h.rec.copy(Platform::Android, "pixel")[0].id, link["id"].as_str().expect("id"), "the missing copy is written");
        assert_eq!(h.rec.copy(Platform::Android, "pixel")[0].rev, 1, "the same record, not a new revision");

        let mut h = both();
        h.rec.refuse_update = vec!["xr".into(), "pixel".into()];
        let e = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect_err("no copy");
        assert!(e.contains("lnk-00000001 wasn't saved"), "{e}");
        assert!(h.rec.forwards.is_empty(), "nothing records the forward, so it went");
        assert_eq!(h.rec.changes(), ["forward xr 8080 -> 41001", "remove xr 41001"]);
    }

    #[test]
    fn a_device_whose_copy_cant_be_read_hides_no_other_link() {
        let mut h = both();
        let link = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect("linked")["link"].clone();
        h.rec.refuse_read = vec!["wear".into()];
        assert_eq!(on(&mut h, "links", json!({})).expect("links"), json!([link]), "wear is skipped, not the listing");
        let id = link["id"].as_str().expect("id");
        assert!(on(&mut h, "link_status", json!({"link": id})).is_ok());
        assert!(on(&mut h, "link", json!({"link": id})).is_ok());
    }

    #[test]
    fn a_member_whose_own_copy_is_gone_finds_its_links_in_its_peers_copies() {
        let mut h = both();
        let link = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect("linked")["link"].clone();
        h.rec.files.remove(&records::fake::key(Platform::Android, "pixel"));
        assert_eq!(on(&mut h, "links", json!({"device": "pixel"})).expect("of pixel"), json!([link]));
        assert_eq!(on(&mut h, "link_status", json!({"device": "pixel"})).expect("status")["link"]["id"], link["id"]);
        let out = on(&mut h, "unlink", json!({"device": "pixel"})).expect("unlinked");
        assert_eq!(out["unlinked"], json!([link["id"]]));
        assert!(h.rec.copy(Platform::Android, "xr").is_empty(), "the peer's copy goes too");
        assert!(h.rec.forwards.is_empty());
    }

    #[test]
    fn unlink_removes_only_forwards_the_adb_server_still_has_as_the_links() {
        let mut h = both();
        let link = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect("linked")["link"].clone();
        // xr rebooted (its forward dropped) and another forward got the host port.
        h.rec.forwards = vec![("xr".into(), 41001, 9999)];
        let calls = h.rec.changes().len();
        let out = on(&mut h, "unlink", json!({"link": link["id"]})).expect("unlinked");
        assert_eq!(out["teardown"], json!([{"id": link["id"]}]), "no error for a forward already gone");
        assert_eq!(h.rec.changes().len(), calls, "the other forward on 41001 is left alone");
        assert_eq!(h.rec.forwards.len(), 1);
    }

    #[test]
    fn link_holds_every_members_link_lock_in_one_order() {
        let mut h = both();
        on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect("linked");
        on(&mut h, "link", json!({"devices": ["pixel", "xr"]})).expect("again");
        assert_eq!(h.rec.held, ["pixel", "xr", "pixel", "xr"], "sorted, whatever order the call names them in");
    }

    #[test]
    fn an_ios_headset_and_phone_link_on_the_host_network() {
        let mut h = both();
        let link = on(&mut h, "link", json!({"devices": [VISION, PHONE]})).expect("linked")["link"].clone();
        assert_eq!((link["kind"].as_str(), link["mechanism"].as_str()), (Some("headset-phone"), Some("network")));
        assert_eq!(link["state"], "active");
        assert!(link.get("forwards").is_none(), "nothing is forwarded on the host network");
        assert_eq!(
            link["members"],
            json!([
                {"device": PHONE, "name": "iPhone 17", "platform": "ios", "role": "phone", "running": true},
                {"device": VISION, "name": "Apple Vision Pro", "platform": "ios", "role": "headset", "running": true},
            ])
        );
        assert!(h.set.calls.is_empty(), "CoreSimulator has no headset pairs: nothing asked of it");
        assert_eq!(h.rec.copy(Platform::Ios, VISION).len(), 1);
        let status = on(&mut h, "link_status", json!({"device": "Apple Vision Pro"})).expect("status");
        let want =
            [("phone exists", true), ("headset exists", true), ("phone running", true), ("headset running", true), ("host network", true)];
        assert_eq!(check_list(&status), want.map(|(n, ok)| (n.to_owned(), ok)));
        let e = on(&mut h, "link", json!({"devices": [VISION, PHONE], "ports": {"Apple Vision Pro": [8080]}})).expect_err("ports");
        assert!(e.starts_with("iOS simulators share the host's network"), "{e}");
        h.set.sims.get_mut(VISION).expect("vision").running = false;
        let listed = on(&mut h, "links", json!({"device": PHONE})).expect("links");
        assert_eq!((listed[0]["state"].as_str(), listed[0]["reason"].as_str()), (Some("pending"), Some("Apple Vision Pro is shut down")));
    }

    #[test]
    fn watch_pairs_and_records_list_unlink_and_report_together() {
        let mut h = both();
        let pair = on(&mut h, "link", json!({"devices": [WATCH, PHONE]})).expect("watch")["link"].clone();
        assert_eq!(pair["mechanism"], "coresim-pair", "a watch still pairs through CoreSimulator");
        let headset = on(&mut h, "link", json!({"devices": [VISION, PHONE]})).expect("headset")["link"].clone();
        let all = on(&mut h, "links", json!({})).expect("links");
        assert_eq!(all, json!([pair, headset]));
        assert_eq!(on(&mut h, "links", json!({"kind": "headset-phone"})).expect("kind"), json!([headset]));
        let e = on(&mut h, "link_status", json!({"device": PHONE})).expect_err("two links");
        assert!(e.starts_with("iPhone 17 is in 2 links (lnk-00000001, FA1A0000-"), "{e}");
        assert_eq!(on(&mut h, "link_status", json!({"device": WATCH})).expect("the pair")["link"]["id"], pair["id"]);
        assert_eq!(on(&mut h, "link_status", json!({"device": VISION})).expect("the record")["link"]["id"], headset["id"]);

        let r = on(&mut h, "unlink", json!({"device": PHONE, "kind": "headset-phone"})).expect("headsets only");
        assert_eq!(r, json!({"unlinked": [headset["id"]], "teardown": [{"id": headset["id"]}]}));
        assert!(h.rec.copy(Platform::Ios, PHONE).is_empty() && h.rec.copy(Platform::Ios, VISION).is_empty());
        assert_eq!(h.set.pairs.len(), 1, "the watch pair stays");
        let r = on(&mut h, "unlink", json!({"device": PHONE})).expect("the rest");
        assert_eq!(r["unlinked"], json!([pair["id"]]));
        assert_eq!(on(&mut h, "links", json!({})).expect("none"), json!([]));
        assert!(on(&mut h, "link_status", json!({"device": VISION})).expect_err("none").contains("Apple Vision Pro is in no link"));
    }

    #[test]
    fn unlink_by_id_removes_the_forwards_and_every_copy() {
        let mut h = both();
        let id = on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect("linked")["link"]["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let r = on(&mut h, "unlink", json!({"link": id})).expect("unlinked");
        assert_eq!(r, json!({"unlinked": [id], "teardown": [{"id": id}]}));
        assert!(h.rec.forwards.is_empty() && h.rec.files.is_empty(), "forwards and both copies gone");
        assert_eq!(h.rec.changes().last().copied(), Some("remove xr 41001"));
        assert!(on(&mut h, "unlink", json!({"link": id})).expect_err("gone").contains(&format!("no link `{id}`")));
        assert_eq!(on(&mut h, "unlink", json!({"device": "xr"})).expect("none"), json!({"unlinked": [], "teardown": []}));
    }

    #[test]
    fn a_member_gone_breaks_the_link_and_a_clones_copy_is_ignored() {
        let mut h = both();
        on(&mut h, "link", json!({"devices": ["xr", "pixel"]})).expect("linked");
        // A clone of pixel copies its file; it isn't a member, so its copy names another device.
        let text = h.rec.files[&records::fake::key(Platform::Android, "pixel")].clone();
        h.rec.files.insert(records::fake::key(Platform::Android, "pixel2"), text);
        assert_eq!(on(&mut h, "links", json!({"device": "pixel2"})).expect("clone"), json!([]));
        assert_eq!(on(&mut h, "links", json!({})).expect("links").as_array().map(Vec::len), Some(1));

        h.rec.devs.retain(|d| d.id != "xr");
        let listed = on(&mut h, "links", json!({})).expect("links");
        assert_eq!((listed[0]["state"].as_str(), listed[0]["reason"].as_str()), (Some("broken"), Some("xr no longer exists")));
        assert_eq!(
            listed[0]["members"][1],
            json!({"device": "xr", "name": null, "platform": "android", "role": "headset", "running": false})
        );
        let e = on(&mut h, "link", json!({"link": "lnk-00000001"})).expect_err("broken");
        assert_eq!(e, "link lnk-00000001 is broken: xr no longer exists; `unlink` it");
        let r = on(&mut h, "unlink", json!({"link": "lnk-00000001"})).expect("unlink clears it");
        assert_eq!(r["unlinked"], json!(["lnk-00000001"]));
    }

    #[test]
    fn the_higher_rev_of_two_copies_wins() {
        let mut h = both();
        on(&mut h, "link", json!({"devices": ["xr", "pixel"], "ports": {"xr": [8080]}})).expect("linked");
        // xr's copy was written by a later call that pixel's write missed.
        let mut newer = h.rec.copy(Platform::Android, "xr");
        newer[0].rev = 7;
        newer[0].params.forwards[0].port = 9000;
        h.rec.files.insert(records::fake::key(Platform::Android, "xr"), records::file_text(&newer).expect("text"));
        let listed = on(&mut h, "links", json!({"device": "pixel"})).expect("of pixel");
        assert_eq!(listed[0]["forwards"][0]["port"], 9000, "pixel's own copy is older");
        // Linking by id again writes the winner back to both.
        on(&mut h, "link", json!({"link": "lnk-00000001"})).expect("again");
        assert_eq!(h.rec.copy(Platform::Android, "pixel")[0].rev, 8);
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
            assert_eq!(crate::platforms(m), Platform::ALL, "{m}: watch pairs on iOS, headset-phone links on both");
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
