//! The links simee keeps itself (docs/design/device-links.md, "Persistence"): every kind but an
//! iOS watch pair, which is CoreSimulator's. Today that is `headset-phone` (#535): on iOS a record
//! with roles and no transport (`network`: simulators already share the host's network), for an
//! Android XR headset and a phone `net-forward` (the ports the members listen on, forwarded to the
//! host with `adb forward`). Android XR glasses pair over Bluetooth and are refused until #435.
//!
//! Each member keeps a copy in its `links.json` (Android: `<avd>/simee/`; iOS: `<data>/simee/`),
//! written under the file's lock (`sim_android::StateLock`) to a temp file and renamed. Of copies
//! that differ the higher `rev` wins; a copy that doesn't name its own device (a clone's) is
//! ignored; a record that doesn't parse is skipped with a warning. The state isn't stored: `links`
//! derives it from the members, `link_status` from probing the forwards as well.

use serde_json::{Value, json};
use sim_core::form_factor::FormFactor;
use sim_core::link::{self, Forward, LinkKind, LinkState, Member, Record, RecordMember, Role};
use sim_core::platform::Platform;

use crate::params::{opt_bool, str_param};

/// Each member's copy of its links, in its state directory.
pub const LINKS_FILE: &str = "links.json";

/// A device as a kept link sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dev {
    pub platform: Platform,
    /// A simulator's UDID, an AVD's name.
    pub id: String,
    pub name: String,
    pub form_factor: Option<FormFactor>,
    /// An Android XR glasses image (`ai-glasses`), not a headset.
    pub glasses: bool,
    pub running: bool,
}

/// The devices, their copies and the adb server's forwards: the backends, or a fake.
pub(crate) trait RecordHost {
    /// The device `id` (exact, any case) of `platform`; `None` when there is none.
    fn device(&mut self, platform: Platform, id: &str) -> Result<Option<Dev>, String>;
    /// Every device on both platforms: for `links` without a device.
    fn all(&mut self) -> Result<Vec<Dev>, String>;
    /// `dev`'s copy as its file has it ([`parse_file`]: only records naming `dev`).
    fn read(&mut self, dev: &Dev) -> Result<Vec<Record>, String>;
    /// Reads `dev`'s copy, applies `change` and writes it back, under the file's lock.
    fn update(&mut self, dev: &Dev, change: &mut dyn FnMut(&mut Vec<Record>)) -> Result<(), String>;
    /// Forwards a free host port to `port` in the running `dev`: the host port.
    fn forward(&mut self, dev: &Dev, port: u16) -> Result<u16, String>;
    fn remove_forward(&mut self, dev: &Dev, host_port: u16) -> Result<(), String>;
    /// The running `dev`'s forwards, (host port, device port) each.
    fn forwards(&mut self, dev: &Dev) -> Result<Vec<(u16, u16)>, String>;
    /// The TCP ports something in the running `dev` listens on.
    fn listening(&mut self, dev: &Dev) -> Result<Vec<u16>, String>;
    /// A new link id: `lnk-` and 8 hex digits.
    fn new_id(&mut self) -> Result<String, String>;
    /// Now, ISO 8601 in UTC to the second.
    fn now(&mut self) -> String;
    /// Holds `dev`'s link-making lock until the value is dropped: `link` takes every member's, in
    /// sorted order, around finding or making the link and saving it, so two calls on one pair
    /// make one link. Not the file's own lock, which each `update` takes inside it.
    fn hold(&mut self, dev: &Dev) -> Result<Box<dyn std::any::Any>, String>;
    /// Says something went wrong that the call carries on past (stderr in the server).
    fn warn(&mut self, message: &str);
}

/// The records in a member's `links.json` that name `own` (a clone's copied file names its
/// original, so it is ignored), with a warning for each one that doesn't parse.
pub(crate) fn parse_file(text: &str, own: (Platform, &str), file: &str) -> (Vec<Record>, Vec<String>) {
    let (mut links, mut warnings) = (Vec::new(), Vec::new());
    let doc: Value = match serde_json::from_str(text) {
        Ok(doc) => doc,
        Err(e) => return (links, vec![format!("{file}: not JSON, every link in it skipped: {e}")]),
    };
    for (i, raw) in doc["links"].as_array().into_iter().flatten().enumerate() {
        let what = raw["id"].as_str().map_or_else(|| format!("link {}", i + 1), |id| format!("link {id}"));
        match serde_json::from_value::<Record>(raw.clone()).map_err(|e| e.to_string()).and_then(|r| r.check().map(|()| r)) {
            Ok(r) if r.has(own.0, own.1) => links.push(r),
            Ok(_) => {}
            Err(e) => warnings.push(format!("{file}: {what} skipped: {e}")),
        }
    }
    (links, warnings)
}

/// A member's `links.json` with `links`; `None` (no file) when there are none.
pub(crate) fn file_text(links: &[Record]) -> Option<String> {
    if links.is_empty() {
        return None;
    }
    serde_json::to_string_pretty(&json!({ "links": links })).ok().map(|mut t| {
        t.push('\n');
        t
    })
}

/// The ports a `link` call names, by member: `ports: {"<device>": [8080, ...]}`, each key a
/// member's id or name. `None` when the call names none.
fn wanted_ports(p: &Value, devs: &[Dev]) -> Result<Option<Vec<(String, u16)>>, String> {
    let Some(ports) = p.get("ports").filter(|v| !v.is_null()) else { return Ok(None) };
    let Some(ports) = ports.as_object() else {
        return Err("`ports` maps a device to the ports it listens on: {\"<device>\": [8080]}".into());
    };
    let mut out = Vec::new();
    for (key, list) in ports {
        let dev = devs.iter().find(|d| d.id.eq_ignore_ascii_case(key) || d.name == *key).ok_or_else(|| {
            let names: Vec<&str> = devs.iter().map(|d| d.name.as_str()).collect();
            format!("`ports` names `{key}`, which isn't one of the link's devices ({})", names.join(", "))
        })?;
        let list = match list {
            Value::Array(a) => a.clone(),
            one => vec![one.clone()],
        };
        for v in list {
            let port = v
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("`ports` for {}: {v} isn't a TCP port (1 to 65535)", dev.name))?;
            if !out.contains(&(dev.id.clone(), port)) {
                out.push((dev.id.clone(), port));
            }
        }
    }
    Ok(Some(out))
}

/// Every record any device holds, merged. A device whose copy can't be read (an AVD whose
/// directory is gone, a config that doesn't parse) is skipped with a warning: one broken device
/// doesn't hide every other link.
pub(crate) fn every(host: &mut dyn RecordHost) -> Result<Vec<Record>, String> {
    let mut all = Vec::new();
    for d in host.all()? {
        match host.read(&d) {
            Ok(copy) => all.extend(copy),
            Err(e) => host.warn(&format!("{}'s links weren't read, so its copy is left out: {e}", d.name)),
        }
    }
    Ok(link::merge(all))
}

/// The records `dev` is in, from every device's copy: a member whose own copy is missing (a
/// write that failed) is still found through its peers'.
pub(crate) fn of_device(host: &mut dyn RecordHost, dev: &Dev) -> Result<Vec<Record>, String> {
    Ok(every(host)?.into_iter().filter(|r| r.has(dev.platform, &dev.id)).collect())
}

/// Runs `erase` (which wipes the simulator's data directory) with the device's `links.json` in
/// `dir` read first and written back after, so an erased member keeps its links (design,
/// "Persistence"). The copy is written back even when `erase` fails, if the file went with it.
pub(crate) fn kept_around<T>(dir: &std::path::Path, erase: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let path = dir.join(LINKS_FILE);
    let saved = std::fs::read(&path).ok();
    let result = erase();
    if let Some(bytes) = saved
        && !path.exists()
    {
        let restored = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, bytes));
        if let Err(e) = restored {
            let note = format!("its links weren't kept: writing {} back failed: {e}", path.display());
            return match result {
                Ok(_) => Err(format!("erased, but {note}")),
                Err(err) => Err(format!("{err}; and {note}")),
            };
        }
    }
    result
}

/// The record `id` names (any case), or the error naming it.
pub(crate) fn find(host: &mut dyn RecordHost, id: &str) -> Result<Record, String> {
    every(host)?.into_iter().find(|r| r.id.eq_ignore_ascii_case(id)).ok_or_else(|| format!("no link `{id}` (`links` lists them)"))
}

/// The record's members as devices, `None` for one that no longer exists, in its order.
fn members_of(host: &mut dyn RecordHost, r: &Record) -> Result<Vec<(RecordMember, Option<Dev>)>, String> {
    let mut out = Vec::new();
    for m in &r.members {
        let dev = match Platform::parse(&m.platform) {
            Some(p) => host.device(p, &m.device)?,
            None => None,
        };
        out.push((m.clone(), dev));
    }
    Ok(out)
}

/// The record's members as devices, or why it is broken (one no longer exists): what linking it
/// again by id needs.
pub(crate) fn existing_members(host: &mut dyn RecordHost, r: &Record) -> Result<Vec<Dev>, String> {
    members_of(host, r)?
        .into_iter()
        .map(|(m, d)| d.ok_or_else(|| format!("link {} is broken: {} no longer exists; `unlink` it", r.id, m.device)))
        .collect()
}

/// Phone first, then the rest as the record has them.
fn phone_first(members: &mut [(RecordMember, Option<Dev>)]) {
    members.sort_by_key(|(m, _)| m.role != Role::Phone.as_str());
}

/// The record as the link object every method returns, its state derived from the members and the
/// forwards saved (`override_state`: what a probe found instead).
fn link_json(host: &mut dyn RecordHost, r: &Record, override_state: Option<(LinkState, String)>) -> Result<Value, String> {
    let mut members = members_of(host, r)?;
    phone_first(&mut members);
    let states: Vec<Member<'_>> = members
        .iter()
        .map(|(m, d)| match d {
            Some(d) if d.running => Member::Running(&d.name),
            Some(d) => Member::Stopped(&d.name),
            None => Member::Missing(&m.device),
        })
        .collect();
    let unforwarded: Vec<String> = r
        .params
        .forwards
        .iter()
        .filter(|f| f.host_port.is_none())
        .filter_map(|f| {
            let d = members.iter().find(|(m, _)| m.device.eq_ignore_ascii_case(&f.device))?.1.as_ref()?;
            d.running.then(|| format!("{}'s port {}", d.name, f.port))
        })
        .collect();
    let (state, reason) = override_state.unwrap_or_else(|| link::record_state(&states, &unforwarded));
    let json_members: Vec<Value> = members
        .iter()
        .map(|(m, d)| {
            json!({
                "device": m.device,
                "name": d.as_ref().map(|d| d.name.as_str()),
                "platform": m.platform,
                "role": m.role,
                "running": d.as_ref().is_some_and(|d| d.running),
            })
        })
        .collect();
    let mut out = json!({
        "id": r.id,
        "kind": r.kind,
        "mechanism": r.mechanism,
        "state": state.as_str(),
        "reason": reason,
        "since": r.since,
        // Only a watch pair has an active one among a phone's links.
        "active": null,
        "members": json_members,
    });
    if r.mechanism == link::NET_FORWARD {
        out["forwards"] = json!(r.params.forwards);
    }
    Ok(out)
}

/// `link` for devices whose kind `plan` found is one simee keeps (`headset-phone`): the link the
/// members already make, set up again, or a new one. Nothing changes before every check passed;
/// a forward this call added is removed again when a later step fails.
pub(crate) fn link(host: &mut dyn RecordHost, kind: LinkKind, devs: &[Dev], roles: &[Role], p: &Value) -> Result<Value, String> {
    if kind != LinkKind::HeadsetPhone {
        return Err(format!("{} links aren't kept yet: only headset-phone (#535)", kind.as_str()));
    }
    if opt_bool(p, "active")?.is_some() {
        return Err("`active` picks a phone's active watch: a headset-phone link has none".into());
    }
    let glasses = devs.iter().find(|d| d.glasses).map(|d| d.name.as_str());
    let mechanism = link::headset_mechanism(devs[0].platform, glasses)?;
    let wanted = wanted_ports(p, devs)?;
    if mechanism == link::HOST_NETWORK && wanted.as_ref().is_some_and(|w| !w.is_empty()) {
        return Err("iOS simulators share the host's network, so there is nothing to forward: a port one listens on is \
                    127.0.0.1:<port> in the other. `ports` is for Android members"
            .into());
    }
    let ids: Vec<(Platform, &str)> = devs.iter().map(|d| (d.platform, d.id.as_str())).collect();
    let mut held = Vec::new();
    for d in sorted(devs) {
        held.push(host.hold(d)?);
    }
    let mut each = Vec::new();
    for d in devs {
        each.push(host.read(d)?);
    }
    let found = link::merge(each.iter().flatten().cloned()).into_iter().find(|r| r.kind == kind.as_str() && r.same_members(&ids));
    // A member whose copy is missing or older (a write that failed) gets the winner back.
    let in_step = found.as_ref().is_some_and(|f| each.iter().all(|copy| copy.iter().any(|r| r.id == f.id && r.rev == f.rev)));
    let before = found.clone();
    let mut record = match found {
        Some(r) => r,
        None => Record {
            id: host.new_id()?,
            kind: kind.as_str().into(),
            mechanism: mechanism.into(),
            rev: 0,
            members: devs
                .iter()
                .zip(roles)
                .map(|(d, r)| RecordMember { platform: d.platform.as_str().into(), device: d.id.clone(), role: r.as_str().into() })
                .collect(),
            params: Default::default(),
            since: None,
        },
    };
    record.mechanism = mechanism.into();
    let added = establish(host, &mut record, devs, wanted)?;
    let changed = before.as_ref() != Some(&record);
    if changed {
        record.rev += 1;
        record.since = Some(host.now());
    }
    if (changed || !in_step)
        && let Err((any_saved, e)) = save(host, &record, devs)
    {
        // Saved nowhere: nothing records the forwards, so they go. Saved with one member, the
        // link exists and holds them; linking again writes the other copy.
        if !any_saved {
            for (dev, host_port) in added {
                let _ = host.remove_forward(&devs[dev], host_port);
            }
        }
        return Err(e);
    }
    Ok(json!({"link": link_json(host, &record, None)?}))
}

/// Sets up the forwards `wanted` asks for (or the record has, when the call names none) on the
/// members that run, keeping the ones the adb server still has, and removes those no longer
/// wanted: the forwards it added, (member index, host port) each.
fn establish(
    host: &mut dyn RecordHost,
    r: &mut Record,
    devs: &[Dev],
    wanted: Option<Vec<(String, u16)>>,
) -> Result<Vec<(usize, u16)>, String> {
    let want = wanted.unwrap_or_else(|| r.params.forwards.iter().map(|f| (f.device.clone(), f.port)).collect());
    let dev_of = |id: &str| devs.iter().position(|d| d.id.eq_ignore_ascii_case(id));
    let mut live: Vec<Option<Vec<(u16, u16)>>> = vec![None; devs.len()];
    let mut added: Vec<(usize, u16)> = Vec::new();
    let mut next = Vec::new();
    let result = (|| -> Result<(), String> {
        for (device, port) in &want {
            let i = dev_of(device).ok_or_else(|| format!("{device} isn't a member"))?;
            let dev = &devs[i];
            let old = r.params.forwards.iter().find(|f| f.device.eq_ignore_ascii_case(device) && f.port == *port).and_then(|f| f.host_port);
            let host_port = if dev.running {
                if live[i].is_none() {
                    live[i] = Some(host.forwards(dev)?);
                }
                match old {
                    Some(h) if live[i].as_ref().is_some_and(|l| l.contains(&(h, *port))) => Some(h),
                    _ => {
                        let h = host.forward(dev, *port).map_err(|e| format!("forwarding {}'s port {port}: {e}", dev.name))?;
                        added.push((i, h));
                        Some(h)
                    }
                }
            } else {
                // The adb server drops a device's forwards when it shuts down.
                None
            };
            next.push(Forward { device: dev.id.clone(), port: *port, host_port });
        }
        Ok(())
    })();
    if let Err(e) = result {
        for (i, h) in added {
            let _ = host.remove_forward(&devs[i], h);
        }
        return Err(e);
    }
    for f in &r.params.forwards {
        let kept = next.iter().any(|n| n.device.eq_ignore_ascii_case(&f.device) && n.port == f.port);
        let (false, Some(h), Some(i)) = (kept, f.host_port, dev_of(&f.device)) else { continue };
        if !devs[i].running {
            continue;
        }
        if live[i].is_none() {
            live[i] = host.forwards(&devs[i]).ok();
        }
        // Only while the adb server still has it as this device's: a host port the kernel has
        // since given another forward isn't this link's to remove. Best effort otherwise: the adb
        // server drops it with the device's next boot anyway.
        if live[i].as_ref().is_some_and(|l| l.contains(&(h, f.port))) {
            let _ = host.remove_forward(&devs[i], h);
        }
    }
    r.params.forwards = next;
    Ok(added)
}

/// `devs` in the order their locks are taken: platform, then id in any case.
fn sorted(devs: &[Dev]) -> Vec<&Dev> {
    let mut order: Vec<&Dev> = devs.iter().collect();
    order.sort_by(|a, b| (a.platform.as_str(), a.id.to_ascii_lowercase()).cmp(&(b.platform.as_str(), b.id.to_ascii_lowercase())));
    order
}

/// Writes `r` into every member's copy, in sorted member order (so two calls take the locks in
/// the same order), replacing the one there. A failure says whether any copy was written.
fn save(host: &mut dyn RecordHost, r: &Record, devs: &[Dev]) -> Result<(), (bool, String)> {
    let (mut saved, mut failed) = (Vec::new(), Vec::new());
    for d in sorted(devs) {
        let mut change = |links: &mut Vec<Record>| {
            links.retain(|l| l.id != r.id);
            links.push(r.clone());
        };
        match host.update(d, &mut change) {
            Ok(()) => saved.push(d.name.as_str()),
            Err(e) => failed.push(format!("{}: {e}", d.name)),
        }
    }
    match (saved.is_empty(), failed.is_empty()) {
        (_, true) => Ok(()),
        (true, false) => Err((false, format!("link {} wasn't saved: {}", r.id, failed.join("; ")))),
        (false, false) => Err((
            true,
            format!("link {} was saved with {} but not with {}: `link` the devices again", r.id, saved.join(" and "), failed.join("; ")),
        )),
    }
}

/// A device's forwards as `adb forward --list` gave them, or why it couldn't.
type Listed = Result<Vec<(u16, u16)>, String>;

/// `unlink` of records: removes each one's forwards (on the members that run) and every member's
/// copy. The ids unlinked, and one teardown entry per record (`error` when a step failed).
pub(crate) fn unlink(host: &mut dyn RecordHost, targets: &[Record]) -> Result<(Vec<Value>, Vec<Value>), String> {
    let (mut unlinked, mut teardown) = (Vec::new(), Vec::new());
    for r in targets {
        let members = members_of(host, r)?;
        let mut errors = Vec::new();
        let mut live: Vec<(String, Listed)> = Vec::new();
        for f in &r.params.forwards {
            let Some(h) = f.host_port else { continue };
            let dev = members.iter().find(|(m, _)| m.device.eq_ignore_ascii_case(&f.device)).and_then(|(_, d)| d.as_ref());
            let Some(dev) = dev.filter(|d| d.running) else { continue };
            if !live.iter().any(|(id, _)| *id == dev.id) {
                live.push((dev.id.clone(), host.forwards(dev)));
            }
            let listed = &live.iter().find(|(id, _)| *id == dev.id).expect("just listed").1;
            match listed {
                // Gone with a reboot or an adb server restart: nothing to tear down, and the host
                // port may be another forward's by now.
                Ok(l) if !l.contains(&(h, f.port)) => {}
                Ok(_) => {
                    if let Err(e) = host.remove_forward(dev, h) {
                        errors.push(format!("removing {}'s forward tcp:{h}: {e}", dev.name));
                    }
                }
                Err(e) => errors.push(format!("listing {}'s forwards: {e}", dev.name)),
            }
        }
        let mut removed = 0;
        let existing: Vec<&Dev> = members.iter().filter_map(|(_, d)| d.as_ref()).collect();
        for dev in &existing {
            match host.update(dev, &mut |links: &mut Vec<Record>| links.retain(|l| l.id != r.id)) {
                Ok(()) => removed += 1,
                Err(e) => errors.push(format!("{}'s copy: {e}", dev.name)),
            }
        }
        if removed > 0 || existing.is_empty() {
            unlinked.push(json!(r.id));
        }
        let mut entry = json!({"id": r.id});
        if !errors.is_empty() {
            entry["error"] = json!(errors.join("; "));
        }
        teardown.push(entry);
    }
    Ok((unlinked, teardown))
}

/// `links` of records (of `device`, of `kind`, in `state`), the state derived from the members.
pub(crate) fn links(
    host: &mut dyn RecordHost,
    device: Option<&Dev>,
    kind: Option<LinkKind>,
    state: Option<LinkState>,
) -> Result<Vec<Value>, String> {
    let records = match device {
        Some(d) => of_device(host, d)?,
        None => every(host)?,
    };
    let mut out = Vec::new();
    for r in records.iter().filter(|r| kind.is_none_or(|k| r.kind == k.as_str())) {
        let l = link_json(host, r, None)?;
        if state.is_none_or(|s| l["state"] == s.as_str()) {
            out.push(l);
        }
    }
    Ok(out)
}

/// `link_status` of a record: each member exists and runs, and for `net-forward` each forward is
/// still the adb server's and something listens on its port. A forward gone, or a port nothing
/// listens on yet, leaves the link `pending` with that as its reason.
pub(crate) fn status(host: &mut dyn RecordHost, r: &Record) -> Result<Value, String> {
    let mut members = members_of(host, r)?;
    phone_first(&mut members);
    let check = |name: String, ok: bool, detail: String| json!({"name": name, "ok": ok, "detail": detail});
    let mut checks = Vec::new();
    for (m, d) in &members {
        let detail = match d {
            Some(d) => format!("{} ({})", d.name, m.device),
            None => format!("{} no longer exists", m.device),
        };
        checks.push(check(format!("{} exists", m.role), d.is_some(), detail));
    }
    for (m, d) in &members {
        let running = d.as_ref().is_some_and(|d| d.running);
        checks.push(check(format!("{} running", m.role), running, if running { "booted" } else { "shut down" }.into()));
    }
    let mut failed: Option<String> = None;
    if r.mechanism == link::HOST_NETWORK {
        checks.push(check(
            "host network".into(),
            true,
            "iOS simulators share the host's network: a port one listens on is 127.0.0.1:<port> in the other".into(),
        ));
    }
    for f in &r.params.forwards {
        let Some((_, Some(dev))) = members.iter().find(|(m, _)| m.device.eq_ignore_ascii_case(&f.device)) else { continue };
        let (name, port) = (&dev.name, f.port);
        if !dev.running {
            checks.push(check(format!("forward {name}:{port}"), false, format!("{name} is shut down")));
            continue;
        }
        let (ok, detail) = match (f.host_port, host.forwards(dev)) {
            (_, Err(e)) => (false, format!("adb forward --list failed: {e}")),
            (None, Ok(_)) => (false, format!("{name}'s port {port} isn't forwarded: `link` the devices again to forward it")),
            (Some(h), Ok(live)) if live.contains(&(h, port)) => {
                (true, format!("127.0.0.1:{h} on the host, 10.0.2.2:{h} from the other device, reaches {name}'s port {port}"))
            }
            (Some(h), Ok(_)) => (
                false,
                format!(
                    "tcp:{h} to {name}'s port {port} is gone (a reboot or an adb server restart drops forwards): `link` the devices again"
                ),
            ),
        };
        if !ok && failed.is_none() {
            failed = Some(detail.clone());
        }
        checks.push(check(format!("forward {name}:{port}"), ok, detail));
        let (ok, detail) = match host.listening(dev) {
            Ok(ports) if ports.contains(&port) => (true, format!("something in {name} listens on {port}")),
            Ok(_) => (false, format!("nothing in {name} listens on {port} yet")),
            Err(e) => (false, format!("reading {name}'s sockets failed: {e}")),
        };
        if !ok && failed.is_none() {
            failed = Some(detail.clone());
        }
        checks.push(check(format!("{name} listens on {port}"), ok, detail));
    }
    let derived = link_json(host, r, None)?;
    let probed = match failed {
        Some(why) if derived["state"] == LinkState::Active.as_str() => Some((LinkState::Pending, why)),
        _ => None,
    };
    let l = if probed.is_some() { link_json(host, r, probed)? } else { derived };
    Ok(json!({"link": l, "checks": checks}))
}

/// Whether `id` is a kept link's (`lnk-…`) rather than a CoreSimulator pair's UUID.
pub(crate) fn is_record_id(id: &str) -> bool {
    id.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("lnk-"))
}

/// The `kind` a call names, if any.
pub(crate) fn opt_kind(p: &Value) -> Result<Option<LinkKind>, String> {
    str_param(p, "kind").map(LinkKind::parse).transpose()
}

/// A scripted [`RecordHost`]: devices on both platforms, each one's `links.json` as text (read
/// and written through [`parse_file`] and [`file_text`], as the real files are), the adb server's
/// forwards and what each device listens on.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    pub(crate) struct FakeRecords {
        pub devs: Vec<Dev>,
        /// `links.json` by `platform:id`.
        pub files: BTreeMap<String, String>,
        /// The adb server's forwards: (device id, host port, device port).
        pub forwards: Vec<(String, u16, u16)>,
        pub listening: BTreeMap<String, Vec<u16>>,
        /// Every forward added or removed and every file written, in order.
        pub calls: Vec<String>,
        /// What adding a forward fails with.
        pub refuse_forward: Option<String>,
        /// The devices whose copies can't be written.
        pub refuse_update: Vec<String>,
        /// The devices whose copies can't be read (an AVD whose directory is gone).
        pub unreadable: Vec<String>,
        pub warnings: Vec<String>,
        next_port: u16,
        next_id: u32,
    }

    pub(crate) fn key(platform: Platform, id: &str) -> String {
        format!("{}:{}", platform.as_str(), id.to_ascii_lowercase())
    }

    impl FakeRecords {
        pub fn with(devs: &[(Platform, &str, FormFactor, bool)]) -> FakeRecords {
            let devs = devs
                .iter()
                .map(|(platform, id, form_factor, running)| Dev {
                    platform: *platform,
                    id: (*id).into(),
                    name: (*id).into(),
                    form_factor: Some(*form_factor),
                    glasses: false,
                    running: *running,
                })
                .collect();
            FakeRecords { devs, ..FakeRecords::default() }
        }

        pub fn dev_mut(&mut self, id: &str) -> &mut Dev {
            self.devs.iter_mut().find(|d| d.id == id).expect("a fake device")
        }

        /// The copy `id` holds, as its file has it.
        pub fn copy(&self, platform: Platform, id: &str) -> Vec<Record> {
            self.files.get(&key(platform, id)).map(|t| parse_file(t, (platform, id), "fake").0).unwrap_or_default()
        }

        pub fn changes(&self) -> Vec<&str> {
            self.calls.iter().map(String::as_str).filter(|c| !c.starts_with("write") && !c.starts_with("hold")).collect()
        }
    }

    impl RecordHost for FakeRecords {
        fn device(&mut self, platform: Platform, id: &str) -> Result<Option<Dev>, String> {
            Ok(self.devs.iter().find(|d| d.platform == platform && d.id.eq_ignore_ascii_case(id)).cloned())
        }
        fn all(&mut self) -> Result<Vec<Dev>, String> {
            Ok(self.devs.clone())
        }
        fn read(&mut self, dev: &Dev) -> Result<Vec<Record>, String> {
            if self.unreadable.contains(&dev.id) {
                return Err(format!("{}'s config can't be read", dev.id));
            }
            Ok(self.copy(dev.platform, &dev.id))
        }
        fn update(&mut self, dev: &Dev, change: &mut dyn FnMut(&mut Vec<Record>)) -> Result<(), String> {
            if self.refuse_update.contains(&dev.id) {
                return Err("read-only file system".into());
            }
            let mut links = self.copy(dev.platform, &dev.id);
            change(&mut links);
            self.calls.push(format!("write {}", dev.id));
            match file_text(&links) {
                Some(text) => self.files.insert(key(dev.platform, &dev.id), text),
                None => self.files.remove(&key(dev.platform, &dev.id)),
            };
            Ok(())
        }
        fn forward(&mut self, dev: &Dev, port: u16) -> Result<u16, String> {
            assert!(dev.running && dev.platform == Platform::Android, "forwards only on a running Android device");
            if let Some(e) = &self.refuse_forward {
                return Err(e.clone());
            }
            let host = 41001 + self.next_port;
            self.next_port += 1;
            self.forwards.push((dev.id.clone(), host, port));
            self.calls.push(format!("forward {} {port} -> {host}", dev.id));
            Ok(host)
        }
        fn remove_forward(&mut self, dev: &Dev, host_port: u16) -> Result<(), String> {
            self.forwards.retain(|(_, h, _)| *h != host_port);
            self.calls.push(format!("remove {} {host_port}", dev.id));
            Ok(())
        }
        fn forwards(&mut self, dev: &Dev) -> Result<Vec<(u16, u16)>, String> {
            Ok(self.forwards.iter().filter(|(d, ..)| *d == dev.id).map(|(_, h, g)| (*h, *g)).collect())
        }
        fn listening(&mut self, dev: &Dev) -> Result<Vec<u16>, String> {
            Ok(self.listening.get(&dev.id).cloned().unwrap_or_default())
        }
        fn new_id(&mut self) -> Result<String, String> {
            self.next_id += 1;
            Ok(format!("lnk-{:08x}", self.next_id))
        }
        fn now(&mut self) -> String {
            "2026-11-28T12:00:00Z".into()
        }
        fn hold(&mut self, dev: &Dev) -> Result<Box<dyn std::any::Any>, String> {
            self.calls.push(format!("hold {}", dev.id));
            Ok(Box::new(()))
        }
        fn warn(&mut self, message: &str) {
            self.warnings.push(message.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, devices: [(&str, &str); 2]) -> Record {
        Record {
            id: id.into(),
            kind: "headset-phone".into(),
            mechanism: link::NET_FORWARD.into(),
            rev: 1,
            members: devices
                .iter()
                .map(|(d, role)| RecordMember { platform: "android".into(), device: (*d).into(), role: (*role).into() })
                .collect(),
            params: Default::default(),
            since: None,
        }
    }

    #[test]
    fn a_file_round_trips_and_keeps_only_good_records_naming_its_device() {
        let good = rec("lnk-00000001", [("pixel", "phone"), ("xr", "headset")]);
        let other = rec("lnk-00000002", [("golden", "phone"), ("xr2", "headset")]);
        let text = file_text(&[good.clone(), other]).expect("text");
        assert!(text.starts_with("{\n  \"links\": [") && text.ends_with("}\n"), "{text}");
        let (links, warnings) = parse_file(&text, (Platform::Android, "PIXEL"), "pixel.avd/simee/links.json");
        assert_eq!(links, std::slice::from_ref(&good), "a record that doesn't name the file's own device (a clone's) is ignored");
        assert!(warnings.is_empty());
        assert_eq!(file_text(&[]), None, "no links: no file");

        let mixed = r#"{"links": [{"id": "lnk-00000001", "kind": "headset-phone", "mechanism": "net-forward", "rev": 1,
            "members": [{"platform": "android", "device": "pixel", "role": "phone"}, {"platform": "android", "device": "xr", "role": "headset"}]},
            {"id": "lnk-00000003", "kind": "pair"}, {"kind": "headset-phone"}]}"#;
        let (links, warnings) = parse_file(mixed, (Platform::Android, "pixel"), "f");
        assert_eq!(links.len(), 1);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("f: link lnk-00000003 skipped"), "{warnings:?}");
        assert!(warnings[1].starts_with("f: link 3 skipped"), "{warnings:?}");
        let (links, warnings) = parse_file("{not json", (Platform::Android, "pixel"), "f");
        assert!(links.is_empty() && warnings[0].contains("not JSON"));
    }

    fn dev(id: &str) -> Dev {
        Dev { platform: Platform::Android, id: id.into(), name: format!("{id} name"), form_factor: None, glasses: false, running: true }
    }

    #[test]
    fn ports_are_named_by_member_id_or_name() {
        let devs = [dev("pixel"), dev("xr")];
        assert_eq!(wanted_ports(&json!({}), &devs), Ok(None));
        assert_eq!(
            wanted_ports(&json!({"ports": {"XR": [8080, 9000, 8080], "pixel name": 5000}}), &devs).map(|w| w.map(|w| w.len())),
            Ok(Some(3))
        );
        let e = wanted_ports(&json!({"ports": {"other": [1]}}), &devs).expect_err("not a member");
        assert_eq!(e, "`ports` names `other`, which isn't one of the link's devices (pixel name, xr name)");
        assert!(wanted_ports(&json!({"ports": {"xr": [0]}}), &devs).expect_err("0").contains("isn't a TCP port"));
        assert!(wanted_ports(&json!({"ports": {"xr": [70000]}}), &devs).expect_err("too big").contains("isn't a TCP port"));
        assert!(wanted_ports(&json!({"ports": [8080]}), &devs).expect_err("a list").contains("maps a device"));
    }

    #[test]
    fn record_ids_are_told_from_pair_uuids() {
        assert!(is_record_id("lnk-0123abcd") && is_record_id("LNK-0123ABCD"));
        assert!(!is_record_id("FA1A0000-0000-4000-8000-000000000001") && !is_record_id("ln"));
    }
}
