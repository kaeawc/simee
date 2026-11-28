//! Pairing a Wear OS emulator with a phone (docs/research/wearos-pairing.md): what the pairing
//! reads from the two guests and decides from it. Pure: `sim-android` runs the adb commands and
//! passes their text in.
//!
//! Stock pairs over TCP through adb, not Bluetooth: a forward on the phone to the companion's
//! port 5601 and a reverse on the watch back to the host's end of it, then a broadcast that makes
//! the watch's Play services dial it, and the phone's companion app started on its
//! `EmulatorActivity`, which logs `[EMULATOR_PAIRING:<state>]` as it goes. The formats and gates
//! here are the ones Android Studio's pairing assistant reads (its `wear-pairing` module).
//!
//! Everything that reaches a guest shell from a caller is checked first: [`package_name`],
//! [`node_id`] and [`emulator_name`] say what they accept.

use std::fmt;

use crate::form_factor::FormFactor;
use crate::link::LinkState;

/// The port the phone's companion app listens on, and the watch's Play services dials.
pub const COMPANION_PORT: u16 = 5601;
/// The first host port a bridge tries: the pair's own end of the forward and the reverse.
pub const FIRST_BRIDGE_PORT: u16 = 5602;

/// Google Play services.
pub const GMS_PACKAGE: &str = "com.google.android.gms";
/// The Pixel Watch app: the companion current Wear OS images ask for.
pub const PIXEL_COMPANION: &str = "com.google.android.apps.wear.companion";
/// The older Wear OS app: the companion when a watch names none. It has no x86_64 build.
pub const LEGACY_COMPANION: &str = "com.google.android.wearable.app";
/// The activity either companion starts the emulator pairing from.
pub const EMULATOR_ACTIVITY: &str = ".EmulatorActivity";

/// The broadcast that makes a guest's Play services reconnect through the bridge.
pub const REFRESH_COMMAND: &str = "am broadcast -a com.google.android.gms.wearable.EMULATOR --es operation refresh-emulator-connection";
/// The broadcast that prints a guest's Data Layer node and its peers.
pub const STATUS_COMMAND: &str = "am broadcast -a com.google.android.gms.wearable.EMULATOR --es operation get-pairing-status";
/// The watch's own answer to which companion it wants: a secure setting, then a system property.
pub const COMPANION_SETTING_COMMAND: &str = "settings get secure cw.oem_companion_package";
pub const COMPANION_PROPERTY_COMMAND: &str = "getprop ro.oem.companion_package";

/// The version code of the legacy companion that has an [`EMULATOR_ACTIVITY`].
pub const LEGACY_EMULATOR_ACTIVITY_VERSION: u32 = 773_385_010;

/// The longest node id a Data Layer node has.
const NODE_ID_MAX: usize = 64;
/// The longest watch name passed to the companion.
const NAME_MAX: usize = 128;

/// `name` if it is an Android package name: two or more dot-separated segments, each starting with
/// a letter and holding letters, digits and `_`. That grammar holds no shell metacharacter.
pub fn package_name(name: &str) -> Result<&str, String> {
    let segment = |s: &str| s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if name.split('.').count() >= 2 && name.split('.').all(segment) {
        Ok(name)
    } else {
        Err(format!("`{name}` isn't a package name (like com.example.app)"))
    }
}

/// `node` if it is a Data Layer node id: `^[a-zA-Z0-9_.-]{1,64}$`.
pub fn node_id(node: &str) -> Result<&str, String> {
    let ok = (1..=NODE_ID_MAX).contains(&node.len()) && node.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if ok {
        Ok(node)
    } else {
        Err(format!("`{}` isn't a Wear OS node id (1 to {NODE_ID_MAX} of a-z, A-Z, 0-9, `_`, `.`, `-`)", clip(node)))
    }
}

/// `name` if it can be the watch's name in the companion: 1 to 128 characters, none a control
/// character. (It is quoted for the guest's shell too: the check is for what the companion shows.)
pub fn emulator_name(name: &str) -> Result<&str, String> {
    let ok = !name.trim().is_empty() && name.chars().count() <= NAME_MAX && !name.chars().any(char::is_control);
    if ok { Ok(name) } else { Err(format!("`{}` isn't a watch name (1 to {NAME_MAX} characters, no control characters)", clip(name))) }
}

/// `s` cut to what an error shows of a guest's or a caller's text.
fn clip(s: &str) -> String {
    let mut out: String = s.chars().take(NODE_ID_MAX).map(|c| if c.is_control() { ' ' } else { c }).collect();
    if s.chars().count() > NODE_ID_MAX {
        out.push('…');
    }
    out
}

/// One peer in a guest's pairing status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// The peer's node id; `None` where the guest printed `null` (a slot with no node yet).
    pub node: Option<String>,
    pub connected: bool,
    pub enabled: bool,
}

/// What `get-pairing-status` printed: the guest's own node and its peers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairingStatus {
    /// The guest's Data Layer node; `None` when the output has none (Play services isn't up yet,
    /// or is older than [`Feature::PairingStatus`]).
    pub local: Option<String>,
    pub peers: Vec<Peer>,
}

impl PairingStatus {
    /// The peer with node `node` (node ids compare ignoring case, as Play services' do).
    pub fn peer(&self, node: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.node.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(node)))
    }

    /// Whether `node` is a peer that is both connected and enabled: the phone's view of a pair
    /// that works.
    pub fn is_connected(&self, node: &str) -> bool {
        self.peer(node).is_some_and(|p| p.connected && p.enabled)
    }
}

/// The text between `tag` (which ends in `[`) and the next `]` on `line`.
fn bracketed<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let rest = &line[line.find(tag)? + tag.len()..];
    Some(&rest[..rest.find(']')?])
}

/// Parses the output of [`STATUS_COMMAND`]. It prints the broadcast's own lines around the data:
///
/// ```text
/// Broadcasting: Intent { act=com.google.android.gms.wearable.EMULATOR flg=0x400000 (has extras) }
/// Broadcast completed: result=1, data="Local:[LocalNodeId]
/// Peer:[TestNodeId,true,true]
/// Peer:[null,false,false]"
/// ```
///
/// Each line is read for itself, wherever it is: a `Local:[<node>]`, then any number of
/// `Peer:[<node>,<connected>,<enabled>]`. A line that doesn't fit (a node that isn't [`node_id`]
/// text, a flag that isn't `true` or `false`) is left out, so nothing from the guest that isn't a
/// node id is handed on. Empty or unrelated text is an empty status.
pub fn parse_pairing_status(out: &str) -> PairingStatus {
    let mut status = PairingStatus::default();
    for line in out.lines() {
        if status.local.is_none()
            && let Some(node) = bracketed(line, "Local:[").and_then(|n| node_id(n).ok())
        {
            status.local = Some(node.to_owned());
            continue;
        }
        let Some(inner) = bracketed(line, "Peer:[") else { continue };
        let parts: Vec<&str> = inner.split(',').collect();
        let [node, connected, enabled] = parts[..] else { continue };
        let flag = |s: &str| match s {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        let node = if node.eq_ignore_ascii_case("null") { Some(None) } else { node_id(node).ok().map(|n| Some(n.to_owned())) };
        if let (Some(node), Some(connected), Some(enabled)) = (node, flag(connected), flag(enabled)) {
            status.peers.push(Peer { node, connected, enabled });
        }
    }
    status
}

/// Where the companion's pairing has got to, from its `[EMULATOR_PAIRING:<state>]` logcat lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingProgress {
    Started,
    /// The companion shows a consent screen a person finishes.
    Consent,
    Pairing,
    Success,
    Failure,
    Cancelled,
    /// A state this list doesn't know (a newer companion): kept by name, never an error.
    Unknown(String),
}

impl PairingProgress {
    /// The state `name` is, as the companion logs it.
    pub fn parse(name: &str) -> PairingProgress {
        match name {
            "STARTED" => PairingProgress::Started,
            "CONSENT" => PairingProgress::Consent,
            "PAIRING" => PairingProgress::Pairing,
            "SUCCESS" => PairingProgress::Success,
            "FAILURE" => PairingProgress::Failure,
            "CANCELLED" => PairingProgress::Cancelled,
            other => PairingProgress::Unknown(clip(other)),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            PairingProgress::Started => "STARTED",
            PairingProgress::Consent => "CONSENT",
            PairingProgress::Pairing => "PAIRING",
            PairingProgress::Success => "SUCCESS",
            PairingProgress::Failure => "FAILURE",
            PairingProgress::Cancelled => "CANCELLED",
            PairingProgress::Unknown(name) => name,
        }
    }

    /// Whether the companion has nothing more to report: it paired, failed or was cancelled.
    pub fn finished(&self) -> bool {
        matches!(self, PairingProgress::Success | PairingProgress::Failure | PairingProgress::Cancelled)
    }
}

/// The state a logcat `line` reports, if it carries an `[EMULATOR_PAIRING:<state>]` marker (any
/// logcat format: the marker is found wherever it is).
pub fn parse_progress_line(line: &str) -> Option<PairingProgress> {
    let name = bracketed(line, "[EMULATOR_PAIRING:")?;
    (!name.is_empty()).then(|| PairingProgress::parse(name))
}

/// Where a companion choice came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompanionSource {
    /// The watch's `cw.oem_companion_package` secure setting.
    Setting,
    /// The watch's `ro.oem.companion_package` property.
    Property,
    /// Neither said: [`LEGACY_COMPANION`].
    Fallback,
}

/// The companion app a watch pairs through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Companion {
    pub package: String,
    pub source: CompanionSource,
    /// What the watch said that was ignored for not being a package name, for the report.
    pub ignored: Vec<String>,
}

/// What one of the watch's answers (a setting's or a property's text) names.
enum Candidate {
    Package(String),
    /// Empty or `null`: the watch has no value.
    Unset,
    Malformed(String),
}

fn candidate(raw: &str) -> Candidate {
    let raw = raw.trim();
    if raw.is_empty() || raw == "null" {
        Candidate::Unset
    } else if package_name(raw).is_ok() {
        Candidate::Package(raw.to_owned())
    } else {
        Candidate::Malformed(clip(raw))
    }
}

/// The companion for a watch: `setting` (the text of [`COMPANION_SETTING_COMMAND`]) if it names a
/// package, else what `property` reads (the text of [`COMPANION_PROPERTY_COMMAND`]; it runs only
/// when the setting didn't decide, and its error is the choice's), else [`LEGACY_COMPANION`]. A
/// value that isn't a package name is ignored, not an error, and listed in `ignored`.
pub fn choose_companion(setting: &str, property: impl FnOnce() -> Result<String, String>) -> Result<Companion, String> {
    let mut ignored = Vec::new();
    match candidate(setting) {
        Candidate::Package(package) => return Ok(Companion { package, source: CompanionSource::Setting, ignored }),
        Candidate::Malformed(m) => ignored.push(format!("cw.oem_companion_package `{m}`")),
        Candidate::Unset => {}
    }
    match candidate(&property()?) {
        Candidate::Package(package) => return Ok(Companion { package, source: CompanionSource::Property, ignored }),
        Candidate::Malformed(m) => ignored.push(format!("ro.oem.companion_package `{m}`")),
        Candidate::Unset => {}
    }
    Ok(Companion { package: LEGACY_COMPANION.to_owned(), source: CompanionSource::Fallback, ignored })
}

/// The ABIs in `ro.product.cpu.abilist` (comma-separated), in order.
pub fn parse_abis(text: &str) -> Vec<String> {
    text.trim().split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned).collect()
}

/// What `dumpsys package <name> | grep -e versionCode -e versionName` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageVersion {
    pub code: Option<u32>,
    pub name: Option<String>,
}

impl PackageVersion {
    /// Whether the package is installed: `dumpsys` printed a version for it.
    pub fn installed(&self) -> bool {
        self.code.is_some() || self.name.is_some()
    }
}

/// The first `versionCode=<digits>` and `versionName=<text>` in `dumpsys package`'s output (it
/// repeats them per user; the first is the package's).
pub fn parse_package_version(out: &str) -> PackageVersion {
    let value = |key: &str| {
        out.lines().find_map(|l| {
            let rest = &l[l.find(key)? + key.len()..];
            Some(rest.split_whitespace().next().unwrap_or("").to_owned())
        })
    };
    PackageVersion {
        code: value("versionCode=").and_then(|v| v.parse().ok()),
        name: value("versionName=").filter(|v| !v.is_empty()).map(|v| clip(&v)),
    }
}

/// A Google Play services capability the pairing uses, and the version that has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// The watch's `adb reverse` listener is reached (the watch needs it).
    ReverseForward,
    /// One phone pairs several watches (the phone needs it).
    MultiWatch,
    /// `get-pairing-status` exists; older Play services say nothing.
    PairingStatus,
    /// `refresh-emulator-connection` exists; older Play services need a restart instead.
    RefreshConnection,
}

impl Feature {
    pub const ALL: [Feature; 4] = [Feature::ReverseForward, Feature::MultiWatch, Feature::PairingStatus, Feature::RefreshConnection];

    /// The first Play services version code with the feature.
    pub const fn min_version(self) -> u32 {
        match self {
            Feature::ReverseForward => 210_915_000,
            Feature::MultiWatch => 213_204_000,
            Feature::PairingStatus => 214_412_000,
            Feature::RefreshConnection => 213_013_000,
        }
    }

    pub const fn what(self) -> &'static str {
        match self {
            Feature::ReverseForward => "reverse port forwarding",
            Feature::MultiWatch => "several watches on one phone",
            Feature::PairingStatus => "the pairing status",
            Feature::RefreshConnection => "refreshing the emulator connection",
        }
    }

    /// Whether Play services at `version` has the feature.
    pub const fn in_version(self, version: u32) -> bool {
        version >= self.min_version()
    }
}

/// Whether the companion `package` at `code` has an [`EMULATOR_ACTIVITY`]: only the legacy
/// companion lacks it, before [`LEGACY_EMULATOR_ACTIVITY_VERSION`] (an unreadable version is old).
pub fn companion_has_emulator_activity(package: &str, code: Option<u32>) -> bool {
    package != LEGACY_COMPANION || code.unwrap_or(0) >= LEGACY_EMULATOR_ACTIVITY_VERSION
}

/// Something that stops a pairing, and what to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub problem: String,
    pub fix: String,
}

impl Problem {
    fn new(problem: impl Into<String>, fix: impl Into<String>) -> Problem {
        Problem { problem: problem.into(), fix: fix.into() }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.problem, self.fix)
    }
}

/// What the preflight needs to know of one member.
#[derive(Clone, Debug)]
pub struct Member<'a> {
    /// What the member is called in messages (its AVD name).
    pub name: &'a str,
    pub form_factor: FormFactor,
    /// The image's API level.
    pub api: u32,
    /// Whether the image has the Play Store (a `google_apis_playstore` image).
    pub play_store: bool,
    /// Google Play services' version code; `None` when it isn't installed or couldn't be read.
    pub gms: Option<u32>,
}

/// The least API a phone image pairs from, and a watch image.
pub const PHONE_MIN_API: u32 = 30;
pub const WATCH_MIN_API: u32 = 28;

const PLAY_IMAGE: &str = "boot an API 30 or newer image with the Play Store (`emu -provision 36 -tag google_apis_playstore`)";

fn update_play_services(name: &str) -> String {
    format!("update Google Play services on {name} (open its Play Store page for it) and run again")
}

/// Every reason `phone` and `watch` can't be paired, each with its fix; empty when they can.
/// Checks the images and Play services (the companion app is [`companion_check`]).
pub fn preflight(phone: &Member<'_>, watch: &Member<'_>) -> Vec<Problem> {
    let mut problems = Vec::new();
    if !matches!(phone.form_factor, FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable) {
        problems.push(Problem::new(
            format!("{} is a {} device, not a phone", phone.name, phone.form_factor.as_str()),
            "pair a watch with a phone, tablet or foldable",
        ));
    } else {
        if phone.api < PHONE_MIN_API {
            problems.push(Problem::new(format!("{} is API {}; a phone pairs from API {PHONE_MIN_API}", phone.name, phone.api), PLAY_IMAGE));
        }
        if !phone.play_store {
            problems.push(Problem::new(format!("{} has no Google Play Store, which the companion app comes from", phone.name), PLAY_IMAGE));
        }
        problems.extend(gms_problem(phone.name, phone.gms, Feature::MultiWatch));
    }
    if watch.form_factor != FormFactor::Watch {
        problems.push(Problem::new(
            format!("{} is a {} device, not a watch", watch.name, watch.form_factor.as_str()),
            "pair a Wear OS emulator (an `android-wear` image)",
        ));
    } else {
        if watch.api < WATCH_MIN_API {
            problems.push(Problem::new(
                format!("{} is API {}; a watch pairs from API {WATCH_MIN_API}", watch.name, watch.api),
                "create the watch from an API 28 or newer Wear OS image",
            ));
        }
        problems.extend(gms_problem(watch.name, watch.gms, Feature::ReverseForward));
    }
    problems
}

/// `name`'s Play services (`version`) against what `feature` needs.
fn gms_problem(name: &str, version: Option<u32>, feature: Feature) -> Option<Problem> {
    let min = feature.min_version();
    match version {
        None => Some(Problem::new(
            format!("Google Play services on {name} isn't installed or couldn't be read (wait for the device to finish booting)"),
            update_play_services(name),
        )),
        Some(v) if !feature.in_version(v) => Some(Problem::new(
            format!("Google Play services {v} on {name} is older than {min}, the first with {}", feature.what()),
            update_play_services(name),
        )),
        Some(_) => None,
    }
}

/// Whether the phone can pair through `companion` (the watch's choice), from what it has:
/// whether the package is installed and its version code, and the phone's ABIs, which only matter
/// when it isn't installed (the legacy companion has no x86_64 build to install).
pub fn companion_check(phone: &str, companion: &str, installed: &PackageVersion, abis: &[String]) -> Result<(), Problem> {
    if installed.installed() {
        return if companion_has_emulator_activity(companion, installed.code) {
            Ok(())
        } else {
            Err(no_emulator_activity(phone, companion))
        };
    }
    if companion != PIXEL_COMPANION && companion != LEGACY_COMPANION {
        return Err(Problem::new(
            format!(
                "the watch asks for the companion app {companion}, which isn't installed on {phone} and isn't one simee knows how to find"
            ),
            format!("install {companion} on {phone} from Google Play, then run again"),
        ));
    }
    if companion == LEGACY_COMPANION && abis == ["x86_64"] {
        return Err(Problem::new(
            format!("the watch asks for {companion}, which has no x86_64 build, and {phone} is x86_64 only"),
            "use an arm64 phone image, or a watch image that asks for the Pixel Watch app",
        ));
    }
    Err(Problem::new(
        format!("the companion app {companion} isn't installed on {phone}"),
        format!("open the Play Store on {phone}, sign in and install it (once per phone image), then run again"),
    ))
}

/// An old companion that can't be started from outside: the person pairs in its own screens.
pub fn no_emulator_activity(phone: &str, companion: &str) -> Problem {
    Problem::new(
        format!("{companion} on {phone} is too old to start the emulator pairing (it has no {EMULATOR_ACTIVITY})"),
        format!("update it in the Play Store, or open it on {phone} and pair the watch by hand"),
    )
}

/// The reason the watch can't pair with the phone whose own node is `phone_node`, when the watch
/// already has a peer that isn't it: a Wear OS emulator pairs with one phone, and leaving it takes
/// a factory reset of the watch. `None` when the watch has no peer, or the phone is its peer.
pub fn watch_peer_conflict(watch: &str, watch_status: &PairingStatus, phone_node: &str) -> Option<Problem> {
    let other = watch_status.peers.iter().find_map(|p| p.node.as_deref().filter(|n| !n.eq_ignore_ascii_case(phone_node)))?;
    Some(Problem::new(
        format!("{watch} is already paired with another phone (node {other})"),
        format!("factory reset {watch} (wipe its data) to pair it again"),
    ))
}

/// What was read of a pair, for [`pair_state`].
#[derive(Clone, Debug)]
pub struct Observed<'a> {
    pub phone: &'a str,
    pub watch: &'a str,
    pub phone_running: bool,
    pub watch_running: bool,
    /// The forward on the phone to [`COMPANION_PORT`] exists.
    pub forward: bool,
    /// The reverse on the watch to the bridge's port exists.
    pub reverse: bool,
    /// Whether the companion is installed on the phone; `None` when it wasn't checked.
    pub companion_installed: Option<bool>,
    /// The watch's node id, when it was read.
    pub watch_node: Option<&'a str>,
    /// The phone's `get-pairing-status`; `None` when it couldn't be read.
    pub status: Option<&'a PairingStatus>,
    /// The last state the companion logged in a handshake, if one ran since the devices started.
    pub progress: Option<&'a PairingProgress>,
}

/// A pair's state with what it says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairState {
    pub state: LinkState,
    /// Why it isn't `active`.
    pub reason: Option<String>,
    /// What a person has to do for it to be, when only a person can (`consent on <phone>`).
    pub needs: Option<String>,
}

impl PairState {
    fn new(state: LinkState, reason: impl Into<String>, needs: Option<String>) -> PairState {
        PairState { state, reason: Some(reason.into()), needs }
    }
}

/// The state of a pair as a function of what was read; never stored. In this order:
///
/// 1. a member that isn't running: `broken`;
/// 2. a forward or reverse that's gone (an adb server restart, a reboot): `broken`, `bridge down`;
/// 3. the companion isn't on the phone: `pending`, needs `companion app on <phone>`;
/// 4. the phone's status lists the watch's node connected and enabled: `active`;
/// 5. the companion's last state: `CONSENT` is `pending` (needs `consent on <phone>`), `STARTED`
///    and `PAIRING` are `pending`, `FAILURE` and `CANCELLED` are `broken`;
/// 6. the watch is a peer that isn't connected: `pending` after `SUCCESS` (it is connecting),
///    else `broken`, `disconnected`;
/// 7. otherwise `pending`: the status doesn't list the watch yet.
pub fn pair_state(o: &Observed<'_>) -> PairState {
    let (phone, watch) = (o.phone, o.watch);
    let down = match (o.phone_running, o.watch_running) {
        (false, false) => Some(format!("{phone} and {watch} aren't running")),
        (false, true) => Some(format!("{phone} isn't running")),
        (true, false) => Some(format!("{watch} isn't running")),
        (true, true) => None,
    };
    if let Some(reason) = down {
        return PairState::new(LinkState::Broken, reason, Some(format!("start {}", if o.phone_running { watch } else { phone })));
    }
    if !o.forward || !o.reverse {
        let gone = match (o.forward, o.reverse) {
            (false, false) => "the forward and the reverse are gone",
            (false, true) => "the forward on the phone is gone",
            _ => "the reverse on the watch is gone",
        };
        return PairState::new(LinkState::Broken, format!("bridge down: {gone}"), Some("rebuild the bridge".to_owned()));
    }
    if o.companion_installed == Some(false) {
        return PairState::new(LinkState::Pending, "the companion app isn't installed", Some(format!("companion app on {phone}")));
    }
    let peer = o.status.zip(o.watch_node).and_then(|(s, n)| s.peer(n));
    if peer.is_some_and(|p| p.connected && p.enabled) {
        return PairState { state: LinkState::Active, reason: None, needs: None };
    }
    match o.progress {
        Some(PairingProgress::Consent) => {
            return PairState::new(LinkState::Pending, "waiting for consent", Some(format!("consent on {phone}")));
        }
        Some(PairingProgress::Started | PairingProgress::Pairing) => {
            return PairState::new(LinkState::Pending, "pairing in progress", None);
        }
        Some(PairingProgress::Failure) => {
            return PairState::new(LinkState::Broken, "the companion reported a failure", Some("pair again".to_owned()));
        }
        Some(PairingProgress::Cancelled) => {
            return PairState::new(LinkState::Broken, "the pairing was cancelled", Some("pair again".to_owned()));
        }
        _ => {}
    }
    if peer.is_some() {
        return if o.progress == Some(&PairingProgress::Success) {
            PairState::new(LinkState::Pending, "paired, waiting for the watch to connect", None)
        } else {
            PairState::new(
                LinkState::Broken,
                "disconnected: the phone lists the watch, not connected",
                Some("pair again if it stays down".to_owned()),
            )
        };
    }
    let why = if o.status.is_none() {
        "the phone's pairing status can't be read yet"
    } else {
        "the phone's pairing status doesn't list the watch yet"
    };
    PairState::new(LinkState::Pending, why, None)
}

#[cfg(test)]
mod tests;
