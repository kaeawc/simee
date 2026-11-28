//! Device links (docs/design/device-links.md): the kinds, the role each member plays, the member
//! rules checked before anything changes, what a CoreSimulator device pair's fields mean as a
//! link's state, and the records simee keeps for the other kinds (`headset-phone`'s mechanisms,
//! `rev` merging, the state derived from the members). Pure: the adapters in `sim-api` do the I/O.

use serde::{Deserialize, Serialize};

use crate::form_factor::FormFactor;
use crate::platform::Platform;

/// What a link joins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// One phone and one watch.
    WatchPhone,
    /// One phone and one car (an Android Automotive AVD).
    CarPhone,
    /// One phone and one headset.
    HeadsetPhone,
    /// 2 to [`MAX_PEERS`] peers on one network, any platforms.
    Network,
}

impl LinkKind {
    pub const ALL: [LinkKind; 4] = [LinkKind::WatchPhone, LinkKind::CarPhone, LinkKind::HeadsetPhone, LinkKind::Network];

    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::WatchPhone => "watch-phone",
            LinkKind::CarPhone => "car-phone",
            LinkKind::HeadsetPhone => "headset-phone",
            LinkKind::Network => "network",
        }
    }

    /// A kind by its name, or the names there are.
    pub fn parse(s: &str) -> Result<LinkKind, String> {
        LinkKind::ALL.into_iter().find(|k| k.as_str() == s).ok_or_else(|| {
            let names: Vec<&str> = LinkKind::ALL.iter().map(|k| k.as_str()).collect();
            format!("unknown link kind `{s}` ({})", names.join(", "))
        })
    }

    /// The role besides `phone` in a pair kind; `None` for a network.
    fn partner(self) -> Option<Role> {
        match self {
            LinkKind::WatchPhone => Some(Role::Watch),
            LinkKind::CarPhone => Some(Role::Car),
            LinkKind::HeadsetPhone => Some(Role::Headset),
            LinkKind::Network => None,
        }
    }
}

/// What a member is to the others in its link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Phone,
    Watch,
    Car,
    Headset,
    Peer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Phone => "phone",
            Role::Watch => "watch",
            Role::Car => "car",
            Role::Headset => "headset",
            Role::Peer => "peer",
        }
    }
}

/// The last state a link was seen in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkState {
    /// Recorded, not carrying anything yet: a member is shut down, or the handshake isn't done.
    Pending,
    /// Carrying traffic.
    Active,
    /// Can't work until someone acts: a member is gone, or the adapter failed.
    Broken,
}

impl LinkState {
    pub const ALL: [LinkState; 3] = [LinkState::Pending, LinkState::Active, LinkState::Broken];

    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Pending => "pending",
            LinkState::Active => "active",
            LinkState::Broken => "broken",
        }
    }

    /// A state by its name, or the names there are.
    pub fn parse(s: &str) -> Result<LinkState, String> {
        LinkState::ALL.into_iter().find(|k| k.as_str() == s).ok_or_else(|| format!("unknown link state `{s}` (pending, active, broken)"))
    }
}

/// The mechanism of an iOS watch pair: CoreSimulator's own device pair.
pub const CORESIM_PAIR: &str = "coresim-pair";

/// The mechanism of an iOS headset-phone link: the host's own network. Simulators are host
/// processes on the host's network stack, so the link carries nothing of its own: it records the
/// pair and its roles.
pub const HOST_NETWORK: &str = "network";

/// The mechanism of an Android XR headset-phone link: the ports its members listen on, each
/// forwarded to the host (`adb forward`), where the other member dials it (10.0.2.2 from an
/// Android guest, 127.0.0.1 from the host).
pub const NET_FORWARD: &str = "net-forward";

/// The issue whose netsimd bridge Android XR glasses wait for: they pair with their phone over
/// Bluetooth (docs/research/headset-pairing.md).
pub const GLASSES_ISSUE: &str = "#435";

/// The mechanism a `headset-phone` link between two devices of `platform` gets, or why none can
/// carry it: `glasses` names a member that is a pair of Android XR glasses (an `ai-glasses` image),
/// which stock pairs with its phone over Bluetooth, and emu's controller has no peers until
/// netsimd's bridge exists.
pub fn headset_mechanism(platform: Platform, glasses: Option<&str>) -> Result<&'static str, String> {
    match (platform, glasses) {
        (Platform::Ios, _) => Ok(HOST_NETWORK),
        (Platform::Android, None) => Ok(NET_FORWARD),
        (Platform::Android, Some(name)) => Err(format!(
            "{name} is a pair of Android XR glasses: glasses pair with their phone over Bluetooth, which waits for emu's \
             netsimd bridge ({GLASSES_ISSUE}); an Android XR headset links with net-forward"
        )),
    }
}

/// A port a link forwards: `device` (a member's id) listens on `port`, and the host reaches it at
/// `host_port` (`None` until the member ran when the link was made).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub device: String,
    pub port: u16,
    #[serde(default)]
    pub host_port: Option<u16>,
}

/// A member as a link record keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordMember {
    /// `ios` or `android`.
    pub platform: String,
    /// Its stable id: a simulator's UDID, an AVD's name.
    pub device: String,
    pub role: String,
}

/// What a mechanism keeps across restarts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<Forward>,
}

/// A link simee keeps itself (every kind but iOS's watch pairs, which are CoreSimulator's): each
/// member's `links.json` holds a copy (docs/design/device-links.md, "Persistence"). Its state isn't
/// kept: every read derives it from the members.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// `lnk-` and 8 hex digits.
    pub id: String,
    pub kind: String,
    pub mechanism: String,
    /// Raised on every write: of two copies that differ, the higher wins.
    #[serde(default)]
    pub rev: u64,
    pub members: Vec<RecordMember>,
    #[serde(default)]
    pub params: Params,
    /// When it was made or last linked again (ISO 8601, UTC).
    #[serde(default)]
    pub since: Option<String>,
}

impl Record {
    /// Why it can't be read as a link: an unknown kind, a member of an unknown platform, an id that
    /// isn't `lnk-` and 8 hex digits, fewer than two members.
    pub fn check(&self) -> Result<(), String> {
        let hex = self.id.strip_prefix("lnk-").is_some_and(|h| h.len() == 8 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        if !hex {
            return Err(format!("`{}` isn't a link id (lnk- and 8 hex digits)", self.id));
        }
        LinkKind::parse(&self.kind)?;
        if self.members.len() < 2 {
            return Err(format!("link {} has {} members", self.id, self.members.len()));
        }
        for m in &self.members {
            if Platform::parse(&m.platform).is_none() {
                return Err(format!("link {}: unknown platform `{}`", self.id, m.platform));
            }
        }
        Ok(())
    }

    /// Whether `(platform, id)` is one of its members (ids in any case).
    pub fn has(&self, platform: Platform, id: &str) -> bool {
        self.members.iter().any(|m| m.platform == platform.as_str() && m.device.eq_ignore_ascii_case(id))
    }

    /// Whether its members are exactly `members`, in any order.
    pub fn same_members(&self, members: &[(Platform, &str)]) -> bool {
        self.members.len() == members.len() && members.iter().all(|(p, id)| self.has(*p, id))
    }
}

/// The copies of several members' files as one list: one record per id, the highest `rev`'s, in
/// the order each id was first seen.
pub fn merge(copies: impl IntoIterator<Item = Record>) -> Vec<Record> {
    let mut out: Vec<Record> = Vec::new();
    for r in copies {
        match out.iter_mut().find(|o| o.id == r.id) {
            Some(o) if r.rev > o.rev => *o = r,
            Some(_) => {}
            None => out.push(r),
        }
    }
    out
}

/// A record link's state from its members alone (what `links` reads, no device I/O): `broken`
/// when one is gone, `pending` while one is shut down or a running member's port wasn't forwarded
/// (`unforwarded`: "`name`'s port 8080"), else `active`.
pub fn record_state(members: &[Member<'_>], unforwarded: &[String]) -> (LinkState, String) {
    if let Some(gone) = members_state(members) {
        return gone;
    }
    if !unforwarded.is_empty() {
        let what = unforwarded.join(" and ");
        return (LinkState::Pending, format!("{what} isn't forwarded: `link` the devices again to forward it"));
    }
    (LinkState::Active, "both run".into())
}

/// `broken` (a member gone) or `pending` (one shut down), with why; `None` when every member runs.
fn members_state(members: &[Member<'_>]) -> Option<(LinkState, String)> {
    let missing: Vec<&str> = members.iter().filter_map(|m| if let Member::Missing(n) = m { Some(*n) } else { None }).collect();
    if !missing.is_empty() {
        let verb = if missing.len() == 1 { "no longer exists" } else { "no longer exist" };
        return Some((LinkState::Broken, format!("{} {verb}", missing.join(" and "))));
    }
    let stopped: Vec<&str> = members.iter().filter_map(|m| if let Member::Stopped(n) = m { Some(*n) } else { None }).collect();
    if !stopped.is_empty() {
        let verb = if stopped.len() == 1 { "is shut down" } else { "are shut down" };
        return Some((LinkState::Pending, format!("{} {verb}", stopped.join(" and "))));
    }
    None
}

/// The most members a `network` link has.
pub const MAX_PEERS: usize = 16;

/// A device as the member rules see it.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    /// Its stable id (a UDID, an AVD name).
    pub id: &'a str,
    /// What errors call it.
    pub name: &'a str,
    pub platform: Platform,
    /// `None` when its device type isn't recognized.
    pub form_factor: Option<FormFactor>,
}

/// The role a device of `form_factor` plays in a `kind` link, or `None` when it can't be in one.
pub fn role(kind: LinkKind, form_factor: FormFactor) -> Option<Role> {
    match (kind, form_factor) {
        (LinkKind::Network, _) => Some(Role::Peer),
        (_, FormFactor::Phone) => Some(Role::Phone),
        (LinkKind::WatchPhone, FormFactor::Watch) => Some(Role::Watch),
        (LinkKind::CarPhone, FormFactor::Auto) => Some(Role::Car),
        (LinkKind::HeadsetPhone, FormFactor::Vision) => Some(Role::Headset),
        _ => None,
    }
}

/// The pair kind two form factors decide when the caller names none: watch + phone, auto + phone
/// or vision + phone, in either order.
pub fn infer(a: FormFactor, b: FormFactor) -> Option<LinkKind> {
    let other = match (a, b) {
        (FormFactor::Phone, other) | (other, FormFactor::Phone) => other,
        _ => return None,
    };
    match other {
        FormFactor::Watch => Some(LinkKind::WatchPhone),
        FormFactor::Auto => Some(LinkKind::CarPhone),
        FormFactor::Vision => Some(LinkKind::HeadsetPhone),
        _ => None,
    }
}

/// "a watch", "an auto".
fn a(form_factor: FormFactor) -> String {
    let s = form_factor.as_str();
    let article = if s.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    format!("{article} {s}")
}

/// "`A` is a watch and `B` is a tablet".
fn what_each_is(members: &[Candidate<'_>], form_factors: &[FormFactor]) -> String {
    let each: Vec<String> = members.iter().zip(form_factors).map(|(m, f)| format!("{} is {}", m.name, a(*f))).collect();
    each.join(" and ")
}

/// Checks `members` for a link of `kind` (or infers the kind from two members' form factors), before
/// anything changes: the kind and each member's role, in `members`' order, or why not, naming what
/// each member is.
pub fn plan(kind: Option<LinkKind>, members: &[Candidate<'_>]) -> Result<(LinkKind, Vec<Role>), String> {
    if members.len() < 2 {
        return Err("a link needs two or more devices (`devices`)".into());
    }
    for (i, m) in members.iter().enumerate() {
        if members[..i].iter().any(|o| o.platform == m.platform && o.id.eq_ignore_ascii_case(m.id)) {
            return Err(format!("{} is named twice: a link joins different devices", m.name));
        }
    }
    let mut form_factors = Vec::with_capacity(members.len());
    for m in members {
        form_factors
            .push(m.form_factor.ok_or_else(|| format!("can't tell what kind of device {} is: its device type isn't recognized", m.name))?);
    }
    let kind = match kind {
        Some(kind) => kind,
        None if members.len() == 2 => infer(form_factors[0], form_factors[1]).ok_or_else(|| {
            format!(
                "{}: no pair kind fits (a phone with a watch, an auto or a vision headset); give kind `network` to put them on one network",
                what_each_is(members, &form_factors)
            )
        })?,
        None => return Err(format!("{} devices make a `network` link: give kind `network`", members.len())),
    };
    let Some(partner) = kind.partner() else {
        if members.len() > MAX_PEERS {
            return Err(format!("a network link has at most {MAX_PEERS} members, not {}", members.len()));
        }
        return Ok((kind, vec![Role::Peer; members.len()]));
    };
    if members.len() != 2 {
        return Err(format!("a {} link has exactly two members, not {}", kind.as_str(), members.len()));
    }
    if members[0].platform != members[1].platform {
        return Err(format!(
            "only network links cross platforms: {} is an {} device and {} an {} one",
            members[0].name,
            members[0].platform.label(),
            members[1].name,
            members[1].platform.label()
        ));
    }
    let roles: Vec<Option<Role>> = form_factors.iter().map(|f| role(kind, *f)).collect();
    let fits = matches!(
        (roles[0], roles[1]),
        (Some(Role::Phone), Some(r)) | (Some(r), Some(Role::Phone)) if r == partner
    );
    if !fits {
        return Err(format!(
            "a {} link pairs one {} with one phone: {}",
            kind.as_str(),
            partner.as_str(),
            what_each_is(members, &form_factors)
        ));
    }
    Ok((kind, roles.into_iter().flatten().collect()))
}

/// One CoreSimulator device pair, as `simctl list pairs` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    /// The pair's UUID: the link's id.
    pub id: String,
    /// The watch's UDID (CoreSimulator's `gizmo`).
    pub watch: String,
    /// The phone's UDID (CoreSimulator's `companion`).
    pub phone: String,
    /// Whether it is its phone's active pair.
    pub active: bool,
    /// Whether CoreSimulator has the pair connected (both booted, the relay up).
    pub connected: bool,
}

impl Pair {
    /// Whether `udid` is one of its two devices.
    pub fn has(&self, udid: &str) -> bool {
        self.watch.eq_ignore_ascii_case(udid) || self.phone.eq_ignore_ascii_case(udid)
    }
}

/// What pairing `watch` with `phone` meets among `pairs`: the same pair already (`Ok(Some)`, to
/// return as it is), nothing (`Ok(None)`), or the pair that already holds the watch with another
/// phone (`Err`): a watch is in at most one watch-phone link.
pub fn existing_pair<'a>(watch: &str, phone: &str, pairs: &'a [Pair]) -> Result<Option<&'a Pair>, &'a Pair> {
    match pairs.iter().find(|p| p.watch.eq_ignore_ascii_case(watch)) {
        Some(p) if p.phone.eq_ignore_ascii_case(phone) => Ok(Some(p)),
        Some(p) => Err(p),
        None => Ok(None),
    }
}

/// A member of a pair as its state is derived: its name and whether it exists and runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Member<'a> {
    Running(&'a str),
    Stopped(&'a str),
    /// Gone: only its UDID is known.
    Missing(&'a str),
}

/// A CoreSimulator pair's state and why: `broken` when a member no longer exists, `pending` while
/// one is shut down or the pair isn't connected yet, `active` when both run and it is connected.
pub fn pair_state(connected: bool, watch: Member<'_>, phone: Member<'_>) -> (LinkState, String) {
    if let Some(gone) = members_state(&[watch, phone]) {
        return gone;
    }
    if connected { (LinkState::Active, "connected".into()) } else { (LinkState::Pending, "both run; the pair isn't connected yet".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ios<'a>(id: &'a str, form_factor: FormFactor) -> Candidate<'a> {
        Candidate { id, name: id, platform: Platform::Ios, form_factor: Some(form_factor) }
    }

    #[test]
    fn names_round_trip() {
        for k in LinkKind::ALL {
            assert_eq!(LinkKind::parse(k.as_str()), Ok(k));
        }
        for s in LinkState::ALL {
            assert_eq!(LinkState::parse(s.as_str()), Ok(s));
        }
        assert!(LinkKind::parse("pair").expect_err("unknown").contains("watch-phone, car-phone, headset-phone, network"));
        assert!(LinkState::parse("up").is_err());
    }

    #[test]
    fn a_watch_and_a_phone_infer_watch_phone_in_either_order() {
        for members in
            [[ios("w", FormFactor::Watch), ios("p", FormFactor::Phone)], [ios("p", FormFactor::Phone), ios("w", FormFactor::Watch)]]
        {
            let (kind, roles) = plan(None, &members).expect("fits");
            assert_eq!(kind, LinkKind::WatchPhone);
            let want: Vec<Role> = members.iter().map(|m| if m.id == "w" { Role::Watch } else { Role::Phone }).collect();
            assert_eq!(roles, want);
        }
        assert_eq!(infer(FormFactor::Auto, FormFactor::Phone), Some(LinkKind::CarPhone));
        assert_eq!(infer(FormFactor::Phone, FormFactor::Vision), Some(LinkKind::HeadsetPhone));
        assert_eq!(infer(FormFactor::Phone, FormFactor::Phone), None);
        assert_eq!(infer(FormFactor::Watch, FormFactor::Tablet), None);
    }

    #[test]
    fn a_watch_with_a_watch_or_an_ipad_is_refused_naming_what_each_is() {
        let e = plan(None, &[ios("W1", FormFactor::Watch), ios("W2", FormFactor::Watch)]).expect_err("two watches");
        assert!(e.contains("W1 is a watch and W2 is a watch") && e.contains("no pair kind fits"), "{e}");
        let e = plan(Some(LinkKind::WatchPhone), &[ios("W1", FormFactor::Watch), ios("W2", FormFactor::Watch)]).expect_err("two watches");
        assert_eq!(e, "a watch-phone link pairs one watch with one phone: W1 is a watch and W2 is a watch");
        let e = plan(Some(LinkKind::WatchPhone), &[ios("W", FormFactor::Watch), ios("iPad", FormFactor::Tablet)]).expect_err("an iPad");
        assert_eq!(e, "a watch-phone link pairs one watch with one phone: W is a watch and iPad is a tablet");
        let e = plan(None, &[ios("W", FormFactor::Watch), ios("iPad", FormFactor::Tablet)]).expect_err("an iPad");
        assert!(e.contains("iPad is a tablet"), "{e}");
        let e = plan(Some(LinkKind::WatchPhone), &[ios("P1", FormFactor::Phone), ios("P2", FormFactor::Phone)]).expect_err("two phones");
        assert!(e.ends_with("P1 is a phone and P2 is a phone"), "{e}");
        let e = plan(Some(LinkKind::CarPhone), &[ios("W", FormFactor::Watch), ios("P", FormFactor::Phone)]).expect_err("not a car");
        assert!(e.starts_with("a car-phone link pairs one car with one phone"), "{e}");
    }

    #[test]
    fn member_counts_platforms_and_unknown_devices_are_refused() {
        assert!(plan(None, &[ios("W", FormFactor::Watch)]).expect_err("one").contains("two or more"));
        let e = plan(None, &[ios("W", FormFactor::Watch), ios("w", FormFactor::Watch)]).expect_err("twice");
        assert!(e.contains("named twice"), "{e}");
        let three = [ios("a", FormFactor::Phone), ios("b", FormFactor::Tablet), ios("c", FormFactor::Watch)];
        assert!(plan(None, &three).expect_err("three").contains("give kind `network`"));
        assert!(plan(Some(LinkKind::WatchPhone), &three).expect_err("three").contains("exactly two"));
        assert_eq!(plan(Some(LinkKind::Network), &three).expect("a network").1, vec![Role::Peer; 3]);
        let ids: Vec<String> = (0..=MAX_PEERS).map(|i| format!("d{i}")).collect();
        let many: Vec<Candidate<'_>> = ids.iter().map(|id| ios(id, FormFactor::Phone)).collect();
        assert!(plan(Some(LinkKind::Network), &many).expect_err("17").contains("at most 16"));
        let wear = Candidate { id: "wear", name: "wear", platform: Platform::Android, form_factor: Some(FormFactor::Watch) };
        let e = plan(None, &[wear, ios("P", FormFactor::Phone)]).expect_err("cross-platform");
        assert!(e.starts_with("only network links cross platforms: wear is an Android device"), "{e}");
        let unknown = Candidate { form_factor: None, ..ios("odd", FormFactor::Phone) };
        assert!(
            plan(None, &[unknown, ios("P", FormFactor::Phone)]).expect_err("unknown").contains("can't tell what kind of device odd is")
        );
    }

    fn pair(id: &str, watch: &str, phone: &str) -> Pair {
        Pair { id: id.into(), watch: watch.into(), phone: phone.into(), active: true, connected: false }
    }

    #[test]
    fn a_watch_is_in_at_most_one_pair_and_the_same_pair_is_found_again() {
        let pairs = [pair("P1", "W1", "PH1"), pair("P2", "W2", "PH1")];
        assert_eq!(existing_pair("w1", "ph1", &pairs).map(|p| p.map(|p| p.id.as_str())), Ok(Some("P1")), "case-insensitive UDIDs");
        assert_eq!(existing_pair("W1", "PH2", &pairs).map_err(|p| p.id.as_str()), Err("P1"), "already paired elsewhere");
        assert_eq!(existing_pair("W3", "PH1", &pairs), Ok(None), "a phone takes several watches");
        assert!(pairs[0].has("ph1") && pairs[0].has("W1") && !pairs[0].has("W2"));
    }

    #[test]
    fn a_pairs_state_is_derived_from_its_members_and_its_connection() {
        use Member::*;
        assert_eq!(pair_state(true, Running("W"), Running("P")), (LinkState::Active, "connected".into()));
        assert_eq!(pair_state(false, Running("W"), Running("P")).0, LinkState::Pending);
        assert_eq!(pair_state(true, Stopped("W"), Running("P")), (LinkState::Pending, "W is shut down".into()));
        assert_eq!(pair_state(false, Stopped("W"), Stopped("P")), (LinkState::Pending, "W and P are shut down".into()));
        assert_eq!(pair_state(false, Missing("W-UDID"), Stopped("P")), (LinkState::Broken, "W-UDID no longer exists".into()));
    }

    #[test]
    fn a_headset_or_glasses_and_a_phone_infer_headset_phone_with_the_headset_role() {
        let headset = Candidate { id: "xr", name: "xr", platform: Platform::Android, form_factor: Some(FormFactor::Vision) };
        let phone = Candidate { id: "pixel", name: "pixel", platform: Platform::Android, form_factor: Some(FormFactor::Phone) };
        assert_eq!(plan(None, &[headset, phone]), Ok((LinkKind::HeadsetPhone, vec![Role::Headset, Role::Phone])));
        let (kind, roles) = plan(None, &[ios("iPhone", FormFactor::Phone), ios("Vision Pro", FormFactor::Vision)]).expect("iOS");
        assert_eq!((kind, roles), (LinkKind::HeadsetPhone, vec![Role::Phone, Role::Headset]));
        let e = plan(Some(LinkKind::HeadsetPhone), &[ios("W", FormFactor::Watch), ios("P", FormFactor::Phone)]).expect_err("a watch");
        assert_eq!(e, "a headset-phone link pairs one headset with one phone: W is a watch and P is a phone");
    }

    #[test]
    fn ios_headsets_share_the_host_network_android_headsets_forward_and_glasses_wait_for_bluetooth() {
        assert_eq!(headset_mechanism(Platform::Ios, None), Ok(HOST_NETWORK));
        assert_eq!(headset_mechanism(Platform::Android, None), Ok(NET_FORWARD));
        let e = headset_mechanism(Platform::Android, Some("glasses")).expect_err("glasses");
        assert!(e.starts_with("glasses is a pair of Android XR glasses") && e.contains("#435"), "{e}");
    }

    fn record(id: &str, rev: u64) -> Record {
        Record {
            id: id.into(),
            kind: "headset-phone".into(),
            mechanism: NET_FORWARD.into(),
            rev,
            members: vec![
                RecordMember { platform: "android".into(), device: "pixel".into(), role: "phone".into() },
                RecordMember { platform: "android".into(), device: "xr".into(), role: "headset".into() },
            ],
            params: Params::default(),
            since: None,
        }
    }

    #[test]
    fn a_record_is_checked_and_matched_by_its_members() {
        let r = record("lnk-0123abcd", 1);
        assert_eq!(r.check(), Ok(()));
        assert!(r.has(Platform::Android, "XR") && !r.has(Platform::Ios, "xr"));
        assert!(r.same_members(&[(Platform::Android, "xr"), (Platform::Android, "pixel")]));
        assert!(!r.same_members(&[(Platform::Android, "xr")]));
        assert!(record("lnk-0123", 1).check().expect_err("short id").contains("isn't a link id"));
        assert!(record("FA1A0000-0000-4000-8000-000000000001", 1).check().is_err(), "a pair's UUID isn't a record's id");
        let mut bad = record("lnk-0123abcd", 1);
        bad.kind = "pair".into();
        assert!(bad.check().expect_err("kind").contains("unknown link kind"));
        bad = record("lnk-0123abcd", 1);
        bad.members[0].platform = "tvos".into();
        assert!(bad.check().expect_err("platform").contains("unknown platform `tvos`"));
        bad.members.truncate(1);
        assert!(bad.check().is_err());
    }

    #[test]
    fn copies_merge_to_the_highest_rev_of_each_link() {
        let merged = merge([record("lnk-00000001", 2), record("lnk-00000002", 1), record("lnk-00000001", 5), record("lnk-00000001", 3)]);
        let got: Vec<(&str, u64)> = merged.iter().map(|r| (r.id.as_str(), r.rev)).collect();
        assert_eq!(got, [("lnk-00000001", 5), ("lnk-00000002", 1)]);
    }

    #[test]
    fn a_records_state_is_derived_from_its_members_and_forwards() {
        use Member::*;
        assert_eq!(record_state(&[Running("P"), Running("XR")], &[]), (LinkState::Active, "both run".into()));
        assert_eq!(record_state(&[Running("P"), Stopped("XR")], &[]), (LinkState::Pending, "XR is shut down".into()));
        assert_eq!(record_state(&[Missing("xr"), Stopped("P")], &[]).0, LinkState::Broken);
        let (state, reason) = record_state(&[Running("P"), Running("XR")], &["XR's port 8080".into()]);
        assert_eq!(state, LinkState::Pending);
        assert!(reason.starts_with("XR's port 8080 isn't forwarded"), "{reason}");
    }

    #[test]
    fn a_record_round_trips_through_serde_with_its_params() {
        let mut r = record("lnk-0123abcd", 4);
        r.params.forwards.push(Forward { device: "xr".into(), port: 8080, host_port: Some(41234) });
        let text = serde_json::to_string(&r).expect("json");
        assert!(text.contains(r#""forwards":[{"device":"xr","port":8080,"host_port":41234}]"#), "{text}");
        assert_eq!(serde_json::from_str::<Record>(&text).expect("parse"), r);
        let bare: Record = serde_json::from_str(
            r#"{"id":"lnk-0123abcd","kind":"headset-phone","mechanism":"network","members":[{"platform":"ios","device":"A","role":"phone"},{"platform":"ios","device":"B","role":"headset"}]}"#,
        )
        .expect("defaults");
        assert_eq!((bare.rev, bare.params, bare.since), (0, Params::default(), None));
    }
}
