//! Pairing a Wear OS emulator with a phone emulator, over adb (docs/research/wearos-pairing.md):
//! the bridge between the two, the companion app's handshake, and taking both down. What the
//! pairing reads and decides is `sim_core::wear`; this is the part that runs adb. Any two devices
//! that have an adb serial work, simee's `emu` and the stock emulator alike.
//!
//! The bridge is the commands Android Studio's pairing assistant runs: a forward on the phone from
//! a free host port (from [`FIRST_BRIDGE_PORT`]) to the companion's port 5601, a reverse on the watch from its own
//! 5601 to that host port, and a broadcast that makes the watch's Play services dial it. The
//! forward lives in the adb server and the reverse in the watch's `adbd`, so an adb server restart
//! or a reboot of either device loses them: [`bridge_with`] again (with the pair's saved port), with
//! no [`handshake`], since Play services keeps the pairing itself.
//!
//! Everything put in a guest shell is checked or quoted first: package names and node ids against
//! their grammar (`sim_core::wear`), the watch's name quoted ([`crate::apps::shell_quote`]).

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use sim_core::wear::{
    self, COMPANION_PORT, Companion, EMULATOR_ACTIVITY, FIRST_BRIDGE_PORT, GMS_PACKAGE, PIXEL_COMPANION, PackageVersion, PairingProgress,
    PairingStatus,
};

use crate::apps::{Adb, am_error, failure, package_name, shell_quote};

/// How many host ports [`bridge`] tries before it gives up.
const BRIDGE_TRIES: usize = 64;
/// How long [`handshake`] waits for the logcat reader's first line before starting the companion
/// anyway: `logcat -T 1` prints the last line at once, so a reader that is silent this long is
/// broken, and the deadline will say so.
const LISTEN_WAIT: Duration = Duration::from_secs(3);

/// How long a [`handshake`] waits for the companion to finish by default: Studio waits a minute
/// before it looks at the status again.
pub const HANDSHAKE_WAIT: Duration = Duration::from_secs(60);

impl Adb {
    /// What `dumpsys package <package>` says of its version; not installed is the default
    /// (nothing found). `package` is checked first.
    pub fn package_version(&self, package: &str) -> Result<PackageVersion, String> {
        let package = package_name(package)?;
        let command = format!("dumpsys package {package} | grep -e versionCode -e versionName");
        match self.run(&["shell", &command], self.deadlines.query)? {
            (true, text) => Ok(wear::parse_package_version(&text)),
            // grep exits 1 when nothing matched: the package isn't there.
            (false, text) if text.trim().is_empty() => Ok(PackageVersion::default()),
            (false, text) => Err(failure(&command, &text)),
        }
    }

    /// Google Play services' version code; `None` when it isn't installed.
    pub fn play_services_version(&self) -> Result<Option<u32>, String> {
        Ok(self.package_version(GMS_PACKAGE)?.code)
    }

    /// The device's ABIs (`ro.product.cpu.abilist`).
    pub fn abis(&self) -> Result<Vec<String>, String> {
        Ok(wear::parse_abis(&self.shell("getprop ro.product.cpu.abilist", self.deadlines.query)?))
    }

    /// The companion app this watch pairs through: its own setting, then its property, then the
    /// older Wear OS app. Run on the watch.
    pub fn companion(&self) -> Result<Companion, String> {
        let setting = self.shell(wear::COMPANION_SETTING_COMMAND, self.deadlines.query)?;
        wear::choose_companion(&setting, || self.shell(wear::COMPANION_PROPERTY_COMMAND, self.deadlines.query))
    }

    /// The device's Data Layer node and peers: Play services' `get-pairing-status`. A status with
    /// no `local` means Play services isn't ready yet (or is older than the status call).
    pub fn pairing_status(&self) -> Result<PairingStatus, String> {
        Ok(wear::parse_pairing_status(&self.shell(wear::STATUS_COMMAND, self.deadlines.query)?))
    }

    /// Makes the device's Play services reconnect through the bridge.
    pub fn refresh_connection(&self) -> Result<(), String> {
        self.shell(wear::REFRESH_COMMAND, self.deadlines.query).map(|_| ())
    }
}

/// Bridges `watch` to `phone`: forwards the first free host port from [`FIRST_BRIDGE_PORT`] on
/// the phone to the companion's 5601, reverses 5601 on the watch to that port, and refreshes the
/// watch's connection. Returns the host port, which the pair keeps to take the bridge down.
/// [`bridge_with`] rebuilds a bridge that is known.
pub fn bridge(phone: &Adb, watch: &Adb) -> Result<u16, String> {
    bridge_with(phone, watch, None)
}

/// [`bridge`] for a pair that may already have a port: after a reboot of the watch or a restart
/// of the adb server one end of the old bridge survives, and a new port would leave it behind.
/// The pair's port is `saved` (what the caller kept), else where the watch's own reverse of 5601
/// still points. It is reused when the phone still forwards it to 5601, bound again when it is
/// free, and replaced by the first free port when another program has taken it. A step that fails
/// takes back what this call did (a forward that was already there stays).
///
/// A port in the adb server's forward table (another pair's, a head unit's) is skipped; one that
/// another program holds on the host is refused by adb and the next is tried.
pub fn bridge_with(phone: &Adb, watch: &Adb, saved: Option<u16>) -> Result<u16, String> {
    let tunnels = phone.forward_tunnels()?;
    let in_use: HashSet<u16> = tunnels.iter().map(|t| t.listen).collect();
    let ours: HashSet<u16> = tunnels.iter().filter(|t| t.owner == phone.serial && t.target == COMPANION_PORT).map(|t| t.listen).collect();
    let known = saved.or_else(|| {
        let reverses = watch.reverse_tunnels().ok()?;
        reverses.iter().find(|t| t.listen == COMPANION_PORT).map(|t| t.target)
    });
    let (port, bound_here) = match known.filter(|p| ours.contains(p)) {
        Some(port) => (port, false),
        None => (forward_free_port(phone, &in_use, known.filter(|p| *p != 0))?, true),
    };
    let rest = watch.reverse(COMPANION_PORT, port).and_then(|()| watch.refresh_connection());
    match rest {
        Ok(()) => Ok(port),
        Err(e) => {
            let _ = watch.remove_reverse(COMPANION_PORT);
            if bound_here {
                let _ = phone.remove_forward(port);
            }
            Err(e)
        }
    }
}

/// Forwards `first` (when given) or else the first port from [`FIRST_BRIDGE_PORT`] that isn't in
/// `in_use` and adb accepts.
fn forward_free_port(phone: &Adb, in_use: &HashSet<u16>, first: Option<u16>) -> Result<u16, String> {
    let mut tried = Vec::new();
    let mut last = String::new();
    let candidates = first.into_iter().chain(FIRST_BRIDGE_PORT..=u16::MAX);
    for port in candidates.filter(|p| !in_use.contains(p)).take(BRIDGE_TRIES) {
        match phone.bind_forward(port, COMPANION_PORT, false) {
            Ok(()) => return Ok(port),
            // adb's refusals of a port: `cannot rebind existing socket`, `cannot bind listener: ...`.
            Err(e) if e.contains("bind") => {
                tried.push(port);
                last = e;
            }
            Err(e) => return Err(e),
        }
    }
    let (first, count) = (tried.first().copied().unwrap_or(FIRST_BRIDGE_PORT), tried.len());
    Err(format!("adb refused every host port it was offered for the bridge ({count} from {first}): {last}"))
}

/// The phone's view of its pairings: Play services' `get-pairing-status`.
pub fn status(phone: &Adb) -> Result<PairingStatus, String> {
    phone.pairing_status()
}

/// How a [`handshake`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// The companion finished: `Success`, `Failure` or `Cancelled`.
    Finished(PairingProgress),
    /// The companion shows its consent screen, which a person finishes on the phone. Reported, not
    /// waited for: check [`status`] later (the pair is `pending`, needing consent on the phone).
    Consent,
    /// The wait ran out with the companion still going.
    Deadline,
}

/// What a [`handshake`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    /// The companion app the watch asked for.
    pub companion: String,
    /// The watch's Data Layer node, which the companion was given.
    pub node: String,
    /// Each state the companion logged, in order.
    pub progress: Vec<PairingProgress>,
    pub end: End,
}

/// Starts the pairing in the phone's companion app and reports how far it gets. Reads the watch's
/// node and companion choice, checks the companion is on the phone (and has an `EmulatorActivity`),
/// opens the phone's logcat, and only once that is listening starts
/// `<companion>/.EmulatorActivity --es emulator-name <watch_name> --es emulator-id <node>`.
/// Each `[EMULATOR_PAIRING:<state>]` line is passed to `report` as it comes; it stops at a
/// finished state, at `CONSENT` (see [`End::Consent`]) or after `wait`. The bridge ([`bridge`])
/// is up first, and the caller has run `sim_core::wear::preflight`.
pub fn handshake(
    phone: &Adb,
    watch: &Adb,
    watch_name: &str,
    wait: Duration,
    mut report: impl FnMut(&PairingProgress),
) -> Result<Handshake, String> {
    let name = wear::emulator_name(watch_name)?;
    let node = watch.pairing_status()?.local.ok_or_else(|| {
        format!(
            "the watch {} has no Data Layer node yet: Google Play services isn't ready (wait for the device to finish booting), \
             or is older than {} (the pairing status)",
            watch.serial,
            wear::Feature::PairingStatus.min_version()
        )
    })?;
    let companion = watch.companion()?.package;
    let installed = phone.package_version(&companion)?;
    if !installed.installed() {
        let problem = wear::companion_check(&phone.serial, &companion, &installed, &phone.abis()?).err();
        return Err(problem.map_or_else(|| format!("{companion} isn't installed on {}", phone.serial), |p| p.to_string()));
    }
    wear::companion_check(&phone.serial, &companion, &installed, &[]).map_err(|p| p.to_string())?;
    let command = emulator_activity_command(&companion, name, &node)?;

    let deadline = Instant::now().checked_add(wait).ok_or_else(|| format!("a wait of {wait:?} is too long"))?;
    let logcat = Logcat::start(phone)?;
    // `logcat -T 1` prints a divider and the last line the log ended with, whatever it was: an
    // earlier pairing's marker, say. Once the reader has shown it is listening, everything it has
    // delivered so far is older than the start below, so none of it is a state of this run.
    wait_until_listening(&logcat.lines);
    while logcat.lines.try_recv().is_ok() {}
    let started = phone.shell(&command, phone.deadlines.start)?;
    if let Some(error) = am_error(&started) {
        return Err(if error.contains("does not exist") || error.contains("not found") {
            wear::no_emulator_activity(&phone.serial, &companion).to_string()
        } else {
            format!("starting {companion}/{EMULATOR_ACTIVITY} on {}: {error}", phone.serial)
        });
    }

    let mut progress = Vec::new();
    let end = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match logcat.lines.recv_timeout(left) {
            Ok(line) => {
                let Some(state) = wear::parse_progress_line(&line) else { continue };
                report(&state);
                progress.push(state.clone());
                if state.finished() {
                    break End::Finished(state);
                }
                if state == PairingProgress::Consent {
                    break End::Consent;
                }
            }
            Err(RecvTimeoutError::Timeout) => break End::Deadline,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!("`adb logcat` on {} ended before the companion finished (is the device still running?)", phone.serial));
            }
        }
    };
    Ok(Handshake { companion, node, progress, end })
}

/// Waits for the first log line that isn't a divider (`--------- beginning of main`: logcat prints
/// one before the first entry of each buffer), for at most [`LISTEN_WAIT`]. Lines other than the
/// divider are what `-T 1` looks back to, so one means the reader is listening.
fn wait_until_listening(lines: &Receiver<String>) {
    let deadline = Instant::now() + LISTEN_WAIT;
    while let Ok(line) = lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if !line.starts_with("--------- ") {
            return;
        }
    }
}

/// The `am start` line for the companion's emulator activity: the component from a checked
/// package, the watch's name and node quoted, the node checked against its grammar.
pub fn emulator_activity_command(companion: &str, watch_name: &str, node: &str) -> Result<String, String> {
    let companion = package_name(companion)?;
    let (name, node) = (wear::emulator_name(watch_name)?, wear::node_id(node)?);
    Ok(format!(
        "am start -n {companion}/{EMULATOR_ACTIVITY} --es emulator-name {} --es emulator-id {}",
        shell_quote(name),
        shell_quote(node)
    ))
}

/// A device's logcat, read line by line from `adb logcat -T 1`; stopped when dropped.
struct Logcat {
    child: Child,
    lines: Receiver<String>,
}

impl Logcat {
    fn start(adb: &Adb) -> Result<Logcat, String> {
        let mut child = Command::new(&adb.adb)
            .args(["-s", &adb.serial, "logcat", "-T", "1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("couldn't run {}: {e}", adb.adb.display()))?;
        let stdout = child.stdout.take().ok_or("adb logcat has no output to read")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            // Bytes, not `lines()`: a guest's log may hold invalid UTF-8, which must not end it.
            while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                if tx.send(String::from_utf8_lossy(&buf).trim_end().to_owned()).is_err() {
                    return;
                }
                buf.clear();
            }
        });
        Ok(Logcat { child, lines })
    }
}

impl Drop for Logcat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What [`unbridge`] did, and what it couldn't.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unbridged {
    /// The phone's forwards removed (5601 and the pair's port, whichever were there).
    pub forwards_removed: Vec<u16>,
    /// Whether the watch's reverse was removed.
    pub reverse_removed: bool,
    /// Whether the companion's data was cleared on the phone.
    pub companion_cleared: bool,
    /// Whether the watch's connection was refreshed.
    pub refreshed: bool,
    /// Each step that failed, with why: the rest still ran.
    pub problems: Vec<String>,
    /// What was left alone on purpose, and why.
    pub notes: Vec<String>,
}

/// Takes a pair's bridge down on whichever devices still run: the phone's forwards of 5601 and of
/// `port` to the companion's port (only those the phone owns), the watch's reverse of 5601 to
/// `port`, the companion's data on the phone when it is the Pixel Watch app (it would reconnect
/// through its cloud sync otherwise), and a refresh of the watch's connection. A device that
/// isn't running (`None`) has nothing to take down, as adb dropped its tunnels with it; one that
/// fails to answer is a [`Unbridged::problems`] entry and the other steps still run.
pub fn unbridge(phone: Option<&Adb>, watch: Option<&Adb>, port: u16, companion: &str) -> Result<Unbridged, String> {
    let companion = package_name(companion)?;
    if port == 0 {
        return Err("a bridge's host port isn't 0".into());
    }
    let (mut problems, mut notes) = (Vec::new(), Vec::new());
    let mut note = |what: &str, e: String| problems.push(format!("{what}: {e}"));
    let mut forwards = Vec::new();
    // The host ports of the phone's other bridges; `None` when they couldn't be listed.
    let mut others = None;
    if let Some(phone) = phone {
        match phone.forwards() {
            Ok(list) => {
                others = Some(
                    list.iter()
                        .filter(|&&(host, device)| device == COMPANION_PORT && host != COMPANION_PORT && host != port)
                        .map(|f| f.0)
                        .collect::<Vec<u16>>(),
                );
                for host in [COMPANION_PORT, port] {
                    if list.contains(&(host, COMPANION_PORT)) {
                        match phone.remove_forward(host) {
                            Ok(()) => forwards.push(host),
                            Err(e) => note(&format!("removing the forward of tcp:{host}"), e),
                        }
                    }
                }
            }
            Err(e) => note("listing the phone's forwards", e),
        }
    }
    let mut reverse_removed = false;
    if let Some(watch) = watch {
        match watch.reverse_tunnels() {
            Ok(list) if list.iter().any(|t| t.listen == COMPANION_PORT && t.target == port) => match watch.remove_reverse(COMPANION_PORT) {
                Ok(()) => reverse_removed = true,
                Err(e) => note("removing the watch's reverse", e),
            },
            Ok(_) => {}
            Err(e) => note("listing the watch's reverses", e),
        }
    }
    let mut cleared = false;
    if let (Some(phone), true) = (phone, companion == PIXEL_COMPANION) {
        if others.as_ref().is_none_or(|o| !o.is_empty()) {
            let why = match &others {
                Some(o) => format!(
                    "the phone is still bridged to another watch (host port {})",
                    o.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
                ),
                None => "the phone's other bridges couldn't be listed".to_owned(),
            };
            notes.push(format!("kept {companion}'s data: {why}, and clearing it unpairs every watch"));
        } else {
            match phone.shell(&format!("pm clear {companion}"), phone.deadlines.query) {
                Ok(text) if text.trim() == "Success" => cleared = true,
                Ok(text) => note(&format!("clearing {companion}"), text.trim().to_owned()),
                Err(e) => note(&format!("clearing {companion}"), e),
            }
        }
    }
    let mut refreshed = false;
    if let Some(watch) = watch {
        match watch.refresh_connection() {
            Ok(()) => refreshed = true,
            Err(e) => note("refreshing the watch's connection", e),
        }
    }
    Ok(Unbridged { forwards_removed: forwards, reverse_removed, companion_cleared: cleared, refreshed, problems, notes })
}

/// Run against `testing`'s fake adb, which is unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::AdbRig;
    use sim_core::wear::{CompanionSource, LEGACY_COMPANION, LEGACY_EMULATOR_ACTIVITY_VERSION};

    const PHONE: &str = "emulator-5554";
    const WATCH: &str = "emulator-5556";
    const REFRESH: &str = "shell am broadcast -a com.google.android.gms.wearable.EMULATOR --es operation refresh-emulator-connection";
    const STATUS: &str = "shell am broadcast -a com.google.android.gms.wearable.EMULATOR --es operation get-pairing-status";
    /// The pair's first host port, and the ones after it.
    const P0: u16 = FIRST_BRIDGE_PORT;
    const SHORT: Duration = Duration::from_millis(400);

    struct Pair {
        rig: AdbRig,
        phone: Adb,
        watch: Adb,
    }

    fn pair(tag: &str) -> Pair {
        let rig = AdbRig::new(tag);
        let (phone, watch) = (rig.adb(PHONE), rig.adb(WATCH));
        Pair { rig, phone, watch }
    }

    impl Pair {
        fn install(&self, serial: &str, package: &str, code: u32) {
            self.rig.fake.wear(
                &format!("pkg.{package}"),
                serial,
                &format!("    versionCode={code} minSdk=30 targetSdk=35\n    versionName=1.0\n"),
            );
        }

        /// A watch that asks for the Pixel companion, installed on the phone, ready to pair.
        fn ready(tag: &str) -> Pair {
            let p = pair(tag);
            p.rig.fake.wear("setting", WATCH, &format!("{PIXEL_COMPANION}\n"));
            p.install(PHONE, PIXEL_COMPANION, 1);
            p.rig.fake.wear_status(WATCH, Some("watch-node"), &[]);
            p
        }

        fn calls(&self) -> Vec<String> {
            self.rig.fake.calls()
        }

        fn handshake(&self, wait: Duration) -> Result<Handshake, String> {
            handshake(&self.phone, &self.watch, "Wear Small", wait, |_| {})
        }
    }

    #[test]
    fn bridge_runs_the_forward_the_reverse_and_the_refresh_in_order() {
        let p = pair("bridge");
        assert_eq!(bridge(&p.phone, &p.watch), Ok(P0));
        assert_eq!(
            p.calls(),
            [
                "forward --list".to_owned(),
                "reverse --list".to_owned(),
                format!("forward --no-rebind tcp:{P0} tcp:5601"),
                format!("reverse tcp:5601 tcp:{P0}"),
                REFRESH.to_owned()
            ]
        );
        assert_eq!(p.rig.fake.forwards(), [format!("{PHONE} tcp:{P0} tcp:5601")]);
        assert_eq!(p.rig.fake.reverses(), [format!("{WATCH} tcp:5601 tcp:{P0}")]);
    }

    #[test]
    fn bridge_skips_host_ports_the_adb_server_already_forwards() {
        let p = pair("skip");
        let other = p.rig.adb("emulator-5558");
        other.bind_forward(P0, 5277, false).expect("another device's forward");
        other.bind_forward(P0 + 1, 5277, false).expect("another");
        assert_eq!(bridge(&p.phone, &p.watch), Ok(P0 + 2));
        // A second watch for the same phone gets its own port, to the same companion port.
        let second = p.rig.adb("emulator-5560");
        assert_eq!(bridge(&p.phone, &second), Ok(P0 + 3));
        assert_eq!(p.rig.fake.reverses(), [format!("{WATCH} tcp:5601 tcp:{}", P0 + 2), format!("emulator-5560 tcp:5601 tcp:{}", P0 + 3)]);
    }

    #[test]
    fn bridge_tries_the_next_port_when_another_program_holds_one() {
        let p = pair("taken");
        p.rig.fake.ports_taken(2);
        assert_eq!(bridge(&p.phone, &p.watch), Ok(P0 + 2));
        let binds: Vec<String> = p.calls().into_iter().filter(|c| c.starts_with("forward --no-rebind")).collect();
        assert_eq!(binds, (0..3).map(|i| format!("forward --no-rebind tcp:{} tcp:5601", P0 + i)).collect::<Vec<_>>());
    }

    #[test]
    fn bridge_gives_up_when_adb_refuses_every_port_and_leaves_nothing_behind() {
        let p = pair("refused");
        p.rig.fake.ports_taken(1000);
        let err = bridge(&p.phone, &p.watch).expect_err("every port taken");
        assert!(err.contains(&format!("64 from {P0}")) && err.contains("Address already in use"), "{err}");
        assert_eq!(p.calls().iter().filter(|c| c.starts_with("forward --no-rebind")).count(), 64);
        assert!(p.rig.fake.forwards().is_empty() && p.rig.fake.reverses().is_empty());
        assert!(!p.calls().iter().any(|c| c.starts_with("reverse tcp") || c.contains("refresh")), "the watch was never changed");
    }

    #[test]
    fn bridge_takes_back_the_forward_when_the_reverse_fails() {
        let p = pair("rollback");
        p.rig.fake.reverse_fails();
        let err = bridge(&p.phone, &p.watch).expect_err("reverse fails");
        assert!(err.starts_with("adb reverse failed:"), "{err}");
        assert!(p.rig.fake.forwards().is_empty(), "the forward is removed again");
        let calls = p.calls();
        assert_eq!(calls[calls.len() - 2..], ["reverse --remove tcp:5601".to_owned(), format!("forward --remove tcp:{P0}")]);
        assert!(!calls.iter().any(|c| c == REFRESH), "no refresh for a bridge that isn't there");
    }

    #[test]
    fn bridge_ports_start_above_the_emulators_console_and_adb_ports() {
        // Console ports run 5554 to 5682 and each one's adb port is the next.
        assert!(FIRST_BRIDGE_PORT > *crate::emu_ports().end() + 1, "{FIRST_BRIDGE_PORT}");
    }

    #[test]
    fn a_farm_of_emulators_does_not_exhaust_the_bridge_ports() {
        let p = pair("farm");
        // 65 emulators hold every console and adb socket from 5554 to 5683: adb can't bind them.
        p.rig.fake.ports_bound(5554, 5683);
        let refused = (5602..5666).filter(|port| p.phone.bind_forward(*port, 5601, false).is_err()).count();
        assert_eq!(refused, 64, "a scan from 5602 would have given up with its 64 tries");
        let mut ports = Vec::new();
        for i in 0..12u16 {
            let watch = p.rig.adb(&format!("emulator-{}", 5556 + 2 * i));
            ports.push(bridge(&p.phone, &watch).expect("a bridge in a farm"));
        }
        assert_eq!(ports, (0..12).map(|i| P0 + i).collect::<Vec<_>>());
        assert!(ports.iter().all(|port| !(5554..=5683).contains(port)));
    }

    #[test]
    fn rebridging_after_a_watch_reboot_reuses_the_saved_forward() {
        let p = pair("reboot");
        let port = bridge(&p.phone, &p.watch).expect("bridge");
        p.watch.remove_reverse(5601).expect("the reboot drops the reverse");
        let before = p.calls().len();
        assert_eq!(bridge_with(&p.phone, &p.watch, Some(port)), Ok(port));
        assert_eq!(p.rig.fake.forwards(), [format!("{PHONE} tcp:{port} tcp:5601")], "the forward is not leaked");
        assert_eq!(p.rig.fake.reverses(), [format!("{WATCH} tcp:5601 tcp:{port}")]);
        let calls = p.calls();
        assert!(!calls[before..].iter().any(|c| c.starts_with("forward --no-rebind")), "{:?}", &calls[before..]);
    }

    #[test]
    fn rebridging_finds_the_port_from_the_watchs_surviving_reverse() {
        let p = pair("surviving");
        let port = bridge(&p.phone, &p.watch).expect("bridge");
        assert_eq!(bridge(&p.phone, &p.watch), Ok(port), "nothing was lost: the same bridge, refreshed");
        assert_eq!(p.rig.fake.forwards().len(), 1);
    }

    #[test]
    fn rebridging_after_the_adb_server_restarted_binds_the_saved_port_again() {
        let p = pair("restart");
        let port = bridge(&p.phone, &p.watch).expect("bridge");
        p.phone.remove_forward(port).expect("the server restart drops the forward");
        p.watch.remove_reverse(5601).expect("and the reverse");
        assert_eq!(bridge_with(&p.phone, &p.watch, Some(port)), Ok(port));
        assert_eq!(p.rig.fake.forwards(), [format!("{PHONE} tcp:{port} tcp:5601")]);
    }

    #[test]
    fn a_saved_port_that_is_now_someone_elses_is_replaced() {
        let p = pair("stolen");
        let saved = P0 + 5;
        p.rig.adb("emulator-5558").bind_forward(saved, 5277, false).expect("another device's forward");
        assert_eq!(bridge_with(&p.phone, &p.watch, Some(saved)), Ok(P0));
        // And one that is a forward of the phone's, but not to the companion's port, is not reused.
        let q = pair("not-companion");
        q.phone.bind_forward(saved, 5277, false).expect("a head unit's forward");
        assert_eq!(bridge_with(&q.phone, &q.watch, Some(saved)), Ok(P0));
    }

    #[test]
    fn a_second_watch_does_not_take_the_first_watchs_forward() {
        let p = pair("two-watches");
        let first = bridge(&p.phone, &p.watch).expect("first");
        let second = p.rig.adb("emulator-5560");
        assert_eq!(bridge(&p.phone, &second), Ok(first + 1));
        assert_eq!(p.rig.fake.forwards().len(), 2);
    }

    #[test]
    fn a_failed_rebridge_leaves_the_forward_that_was_already_there() {
        let p = pair("rebridge-fails");
        let port = bridge(&p.phone, &p.watch).expect("bridge");
        p.watch.remove_reverse(5601).expect("reboot");
        p.rig.fake.reverse_fails();
        assert!(bridge_with(&p.phone, &p.watch, Some(port)).is_err());
        assert_eq!(p.rig.fake.forwards().len(), 1, "the earlier forward is not this call's to remove");
    }

    #[test]
    fn the_listening_wait_skips_buffer_dividers() {
        let (tx, rx) = mpsc::channel();
        for line in ["--------- beginning of main", "--------- beginning of system", "11-28 10:00:00.000 1 1 I tag: look-back", "later"] {
            tx.send(line.to_owned()).expect("send");
        }
        let started = Instant::now();
        wait_until_listening(&rx);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(rx.try_recv().as_deref(), Ok("later"), "it stopped at the look-back line, not at a divider");
        // A reader that ends with only a divider is not waited for.
        let (tx, rx) = mpsc::channel();
        tx.send("--------- beginning of main".to_owned()).expect("send");
        drop(tx);
        wait_until_listening(&rx);
    }

    #[test]
    fn unbridging_one_watch_keeps_the_companion_for_the_others() {
        let p = pair("shared-phone");
        let second = p.rig.adb("emulator-5560");
        let (a, b) = (bridge(&p.phone, &p.watch).expect("a"), bridge(&p.phone, &second).expect("b"));
        let done = unbridge(Some(&p.phone), Some(&p.watch), a, PIXEL_COMPANION).expect("unbridge a");
        assert!(!done.companion_cleared && done.reverse_removed && done.forwards_removed == [a], "{done:?}");
        assert_eq!(done.notes.len(), 1);
        assert!(done.notes[0].contains(PIXEL_COMPANION) && done.notes[0].contains(&b.to_string()), "{:?}", done.notes);
        assert!(!p.calls().iter().any(|c| c.contains("pm clear")), "{:?}", p.calls());
        assert_eq!(p.rig.fake.forwards(), [format!("{PHONE} tcp:{b} tcp:5601")], "the other watch's bridge stands");
        // The last one out clears it.
        let done = unbridge(Some(&p.phone), Some(&second), b, PIXEL_COMPANION).expect("unbridge b");
        assert!(done.companion_cleared && done.notes.is_empty(), "{done:?}");
    }

    #[test]
    fn a_failed_adb_listing_stops_the_bridge_before_it_binds_anything() {
        let p = pair("nolist");
        let adb = Adb { adb: p.rig.root.join("missing-adb"), ..p.phone.clone() };
        assert!(bridge(&adb, &p.watch).expect_err("no adb").contains("couldn't run"));
        assert!(p.calls().is_empty());
    }

    #[test]
    fn status_is_the_phones_parsed_pairing_status() {
        let p = pair("status");
        p.rig.fake.wear_status(PHONE, Some("phone-node"), &[("watch-node", true, true), ("other", false, false)]);
        let s = status(&p.phone).expect("status");
        assert_eq!(s.local.as_deref(), Some("phone-node"));
        assert!(s.is_connected("watch-node") && !s.is_connected("other"));
        assert_eq!(p.calls(), [STATUS]);
    }

    #[test]
    fn status_before_play_services_is_ready_has_no_local_node() {
        let p = pair("notready");
        p.rig.fake.wear_status(PHONE, None, &[]);
        assert_eq!(status(&p.phone).expect("status").local, None);
    }

    #[test]
    fn package_versions_and_abis_are_read_from_the_guest() {
        let p = pair("facts");
        p.install(PHONE, PIXEL_COMPANION, 773_000_000);
        let v = p.phone.package_version(PIXEL_COMPANION).expect("version");
        assert_eq!((v.code, v.name.as_deref()), (Some(773_000_000), Some("1.0")));
        assert!(!p.phone.package_version(LEGACY_COMPANION).expect("absent").installed());
        assert_eq!(p.phone.play_services_version(), Ok(Some(240_000_000)));
        assert_eq!(p.phone.abis(), Ok(vec!["arm64-v8a".to_owned()]));
        assert!(p.phone.package_version("com.x;reboot").is_err());
        assert_eq!(p.calls().len(), 4, "the bad name never reached adb: {:?}", p.calls());
    }

    #[test]
    fn the_watchs_companion_is_its_setting_then_its_property_then_the_old_app() {
        let p = pair("choice");
        let c = p.watch.companion().expect("fallback");
        assert_eq!((c.package.as_str(), c.source), (LEGACY_COMPANION, CompanionSource::Fallback));
        assert_eq!(p.calls(), ["shell settings get secure cw.oem_companion_package", "shell getprop ro.oem.companion_package"]);
        p.rig.fake.wear("prop", WATCH, "com.vendor.watch\n");
        assert_eq!(p.watch.companion().expect("property").source, CompanionSource::Property);
        p.rig.fake.wear("setting", WATCH, &format!("{PIXEL_COMPANION}\n"));
        let before = p.calls().len();
        let c = p.watch.companion().expect("setting");
        assert_eq!((c.package.as_str(), c.source), (PIXEL_COMPANION, CompanionSource::Setting));
        assert_eq!(p.calls().len() - before, 1, "the property isn't read when the setting decides");
    }

    #[test]
    fn the_activity_command_validates_and_quotes() {
        assert_eq!(
            emulator_activity_command(PIXEL_COMPANION, "Wear Small", "node-1"),
            Ok(format!("am start -n {PIXEL_COMPANION}/.EmulatorActivity --es emulator-name 'Wear Small' --es emulator-id 'node-1'"))
        );
        let hostile = emulator_activity_command(PIXEL_COMPANION, "it's \"x\"; reboot", "n").expect("quoted");
        assert!(hostile.contains(r#"'it'\''s "x"; reboot'"#), "{hostile}");
        assert!(emulator_activity_command("com.x;reboot", "w", "n").is_err());
        assert!(emulator_activity_command(PIXEL_COMPANION, "w", "n'; reboot; '").is_err());
        assert!(emulator_activity_command(PIXEL_COMPANION, "a\nb", "n").is_err());
        assert!(emulator_activity_command(PIXEL_COMPANION, "", "n").is_err());
    }

    #[test]
    fn handshake_starts_the_companion_after_the_log_reader_and_reports_every_state() {
        let p = Pair::ready("shake");
        p.rig.fake.wear_logcat(PHONE, &["STARTED", "PAIRING", "SUCCESS"]);
        let mut seen = Vec::new();
        let h = handshake(&p.phone, &p.watch, "Wear Small", Duration::from_secs(10), |s| seen.push(s.clone())).expect("handshake");
        assert_eq!(
            p.calls(),
            [
                STATUS.to_owned(),
                "shell settings get secure cw.oem_companion_package".to_owned(),
                format!("shell dumpsys package {PIXEL_COMPANION} | grep -e versionCode -e versionName"),
                "logcat -T 1".to_owned(),
                format!(
                    "shell am start -n {PIXEL_COMPANION}/.EmulatorActivity --es emulator-name 'Wear Small' --es emulator-id 'watch-node'"
                ),
            ]
        );
        let states = vec![PairingProgress::Started, PairingProgress::Pairing, PairingProgress::Success];
        assert_eq!(h.progress, states);
        assert_eq!(seen, states, "each state was reported as it came");
        assert_eq!(h.end, End::Finished(PairingProgress::Success));
        assert_eq!((h.companion.as_str(), h.node.as_str()), (PIXEL_COMPANION, "watch-node"));
    }

    #[test]
    fn handshake_stops_at_failure_and_at_cancelled() {
        for state in ["FAILURE", "CANCELLED"] {
            let p = Pair::ready("finish");
            p.rig.fake.wear_logcat(PHONE, &["STARTED", state, "SUCCESS"]);
            let h = p.handshake(Duration::from_secs(10)).expect("handshake");
            let last = PairingProgress::parse(state);
            assert_eq!(h.progress, [PairingProgress::Started, last.clone()], "{state}: nothing after it");
            assert_eq!(h.end, End::Finished(last));
        }
    }

    #[test]
    fn handshake_reports_consent_and_does_not_wait_it_out() {
        let p = Pair::ready("consent");
        p.rig.fake.wear_logcat(PHONE, &["STARTED", "CONSENT", "PAIRING", "SUCCESS"]);
        let started = Instant::now();
        let h = p.handshake(Duration::from_secs(30)).expect("handshake");
        assert!(started.elapsed() < Duration::from_secs(10), "returned at consent, not at the wait");
        assert_eq!(h.progress, [PairingProgress::Started, PairingProgress::Consent]);
        assert_eq!(h.end, End::Consent);
    }

    #[test]
    fn handshake_keeps_unknown_states_and_goes_on() {
        let p = Pair::ready("unknown");
        p.rig.fake.wear_logcat(PHONE, &["WARMING_UP", "SUCCESS"]);
        let h = p.handshake(Duration::from_secs(10)).expect("handshake");
        assert_eq!(h.progress, [PairingProgress::Unknown("WARMING_UP".into()), PairingProgress::Success]);
        assert_eq!(h.end, End::Finished(PairingProgress::Success));
    }

    #[test]
    fn handshake_ends_at_its_deadline_with_what_it_saw() {
        let p = Pair::ready("deadline");
        p.rig.fake.wear_logcat(PHONE, &["STARTED", "PAIRING"]);
        let started = Instant::now();
        let h = p.handshake(SHORT).expect("handshake");
        assert_eq!(h.end, End::Deadline);
        assert_eq!(h.progress, [PairingProgress::Started, PairingProgress::Pairing]);
        assert!(started.elapsed() < Duration::from_secs(10));
        // A companion that logs nothing at all ends the same way.
        let quiet = Pair::ready("quiet");
        let h = quiet.handshake(SHORT).expect("handshake");
        assert_eq!((h.end, h.progress.len()), (End::Deadline, 0));
    }

    #[test]
    fn handshake_errors_when_the_log_ends_before_the_companion_finishes() {
        let p = Pair::ready("logend");
        p.rig.fake.wear_logcat(PHONE, &["STARTED"]);
        p.rig.fake.wear("logcat.exit", PHONE, "");
        let err = p.handshake(Duration::from_secs(10)).expect_err("log ended");
        assert!(err.contains("`adb logcat` on emulator-5554 ended before the companion finished"), "{err}");
        // A log that ends after a finished state is not an error: the state came first.
        let p = Pair::ready("logend-done");
        p.rig.fake.wear_logcat(PHONE, &["STARTED", "SUCCESS"]);
        p.rig.fake.wear("logcat.exit", PHONE, "");
        assert_eq!(p.handshake(Duration::from_secs(10)).map(|h| h.end), Ok(End::Finished(PairingProgress::Success)));
    }

    #[test]
    fn handshake_does_not_take_the_look_back_line_for_a_state() {
        let p = Pair::ready("primer");
        // The last line of the phone's log, from an earlier pairing, is what `logcat -T 1` prints first.
        p.rig.fake.wear("primer", PHONE, "08-16 09:00:00.000   439  2734 I EmulatorActivity:[EMULATOR_PAIRING:SUCCESS]\n");
        p.rig.fake.wear_logcat(PHONE, &["STARTED", "FAILURE"]);
        let h = p.handshake(Duration::from_secs(10)).expect("handshake");
        assert_eq!(h.progress, [PairingProgress::Started, PairingProgress::Failure]);
        assert_eq!(h.end, End::Finished(PairingProgress::Failure));
    }

    #[test]
    fn handshake_with_the_companion_missing_says_to_install_it_and_starts_nothing() {
        let p = pair("nocompanion");
        p.rig.fake.wear("setting", WATCH, &format!("{PIXEL_COMPANION}\n"));
        let err = p.handshake(SHORT).expect_err("not installed");
        assert!(err.contains(PIXEL_COMPANION) && err.contains("isn't installed") && err.contains("Play Store"), "{err}");
        let calls = p.calls();
        assert!(!calls.iter().any(|c| c.starts_with("logcat") || c.contains("am start")), "{calls:?}");
        assert_eq!(calls.last().map(String::as_str), Some("shell getprop ro.product.cpu.abilist"));
    }

    #[test]
    fn handshake_explains_the_legacy_companions_missing_x86_64_build() {
        let p = pair("abi");
        p.rig.fake.wear("abis", PHONE, "x86_64\n");
        let err = p.handshake(SHORT).expect_err("no build");
        assert!(err.contains(LEGACY_COMPANION) && err.contains("no x86_64 build"), "{err}");
    }

    #[test]
    fn handshake_with_an_old_companion_says_to_pair_by_hand() {
        let p = pair("old");
        p.install(PHONE, LEGACY_COMPANION, LEGACY_EMULATOR_ACTIVITY_VERSION - 1);
        let err = p.handshake(SHORT).expect_err("too old");
        assert!(err.contains(".EmulatorActivity") && err.contains("pair the watch by hand"), "{err}");
        assert!(!p.calls().iter().any(|c| c.starts_with("logcat")), "{:?}", p.calls());
        // The legacy companion at the gate's version starts.
        let p = pair("gate");
        p.install(PHONE, LEGACY_COMPANION, LEGACY_EMULATOR_ACTIVITY_VERSION);
        p.rig.fake.wear_status(WATCH, Some("watch-node"), &[]);
        p.rig.fake.wear_logcat(PHONE, &["SUCCESS"]);
        assert_eq!(p.handshake(Duration::from_secs(10)).map(|h| h.end), Ok(End::Finished(PairingProgress::Success)));
    }

    #[test]
    fn handshake_with_no_emulator_activity_in_the_companion_says_to_pair_by_hand() {
        let p = Pair::ready("noactivity");
        p.rig.fake.wear("noactivity", PHONE, "");
        let err = p.handshake(SHORT).expect_err("no activity");
        assert!(err.contains("pair the watch by hand") && err.contains(PIXEL_COMPANION), "{err}");
    }

    #[test]
    fn handshake_needs_the_watchs_node() {
        let p = Pair::ready("nonode");
        p.rig.fake.wear_status(WATCH, None, &[]);
        let err = p.handshake(SHORT).expect_err("no node");
        assert!(err.contains("isn't ready") && err.contains("214412000"), "{err}");
        assert_eq!(p.calls(), [STATUS], "nothing else ran");
    }

    #[test]
    fn nothing_hostile_from_the_guest_or_the_caller_reaches_a_shell() {
        // A watch node that isn't a node id is no node.
        let p = Pair::ready("hostile-node");
        p.rig.fake.wear("status", WATCH, "Broadcast completed: result=1, data=\"Local:[x'; reboot; echo ']\"\n");
        assert!(p.handshake(SHORT).is_err());
        assert!(!p.calls().iter().any(|c| c.contains("reboot")), "{:?}", p.calls());
        // A companion name that isn't a package name is ignored for the old app.
        let p = pair("hostile-companion");
        p.rig.fake.wear("setting", WATCH, "com.x; reboot\n");
        p.rig.fake.wear("prop", WATCH, "$(reboot)\n");
        let _ = p.handshake(SHORT);
        assert!(!p.calls().iter().any(|c| c.contains("reboot") && !c.contains("get")), "{:?}", p.calls());
        assert!(p.calls().iter().any(|c| c.contains(LEGACY_COMPANION)), "{:?}", p.calls());
        // A watch name with a newline never reaches adb.
        let p = Pair::ready("hostile-name");
        assert!(handshake(&p.phone, &p.watch, "a\nb", SHORT, |_| {}).is_err());
        assert!(p.calls().is_empty());
    }

    /// A bridged pair, a stray forward of 5601 on the phone (the older recipe's), and the
    /// phone's `calls()` since.
    fn bridged(tag: &str) -> (Pair, u16) {
        let p = pair(tag);
        p.phone.bind_forward(5601, 5601, false).expect("older recipe's forward");
        let port = bridge(&p.phone, &p.watch).expect("bridge");
        (p, port)
    }

    #[test]
    fn unbridge_takes_everything_down_in_order() {
        let (p, port) = bridged("down");
        assert_eq!(port, P0);
        let before = p.calls().len();
        let done = unbridge(Some(&p.phone), Some(&p.watch), port, PIXEL_COMPANION).expect("unbridge");
        assert_eq!(
            done,
            Unbridged {
                forwards_removed: vec![5601, P0],
                reverse_removed: true,
                companion_cleared: true,
                refreshed: true,
                problems: vec![],
                notes: vec![]
            }
        );
        assert_eq!(
            p.calls()[before..],
            [
                "forward --list".to_owned(),
                "forward --remove tcp:5601".to_owned(),
                format!("forward --remove tcp:{P0}"),
                "reverse --list".to_owned(),
                "reverse --remove tcp:5601".to_owned(),
                format!("shell pm clear {PIXEL_COMPANION}"),
                REFRESH.to_owned(),
            ]
        );
        assert!(p.rig.fake.forwards().is_empty() && p.rig.fake.reverses().is_empty());
    }

    #[test]
    fn unbridge_clears_only_the_pixel_companion() {
        let (p, port) = bridged("legacy");
        let done = unbridge(Some(&p.phone), Some(&p.watch), port, LEGACY_COMPANION).expect("unbridge");
        assert!(!done.companion_cleared);
        assert!(!p.calls().iter().any(|c| c.contains("pm clear")), "{:?}", p.calls());
    }

    #[test]
    fn unbridge_with_the_watch_stopped_still_clears_the_phone() {
        let (p, port) = bridged("nowatch");
        let before = p.calls().len();
        let done = unbridge(Some(&p.phone), None, port, PIXEL_COMPANION).expect("unbridge");
        assert_eq!(
            done,
            Unbridged {
                forwards_removed: vec![5601, P0],
                reverse_removed: false,
                companion_cleared: true,
                refreshed: false,
                problems: vec![],
                notes: vec![]
            }
        );
        assert!(p.calls()[before..].iter().all(|c| c.starts_with("forward") || c.contains("pm clear")), "{:?}", p.calls());
    }

    #[test]
    fn unbridge_with_the_phone_stopped_still_takes_down_the_watch() {
        let (p, port) = bridged("nophone");
        let before = p.calls().len();
        let done = unbridge(None, Some(&p.watch), port, PIXEL_COMPANION).expect("unbridge");
        assert_eq!(
            done,
            Unbridged {
                forwards_removed: vec![],
                reverse_removed: true,
                companion_cleared: false,
                refreshed: true,
                problems: vec![],
                notes: vec![]
            }
        );
        assert_eq!(p.calls()[before..], ["reverse --list".to_owned(), "reverse --remove tcp:5601".to_owned(), REFRESH.to_owned()]);
        assert!(p.rig.fake.reverses().is_empty());
    }

    #[test]
    fn unbridge_with_both_stopped_does_nothing() {
        let p = pair("neither");
        assert_eq!(unbridge(None, None, P0, PIXEL_COMPANION), Ok(Unbridged::default()));
        assert!(p.calls().is_empty());
    }

    #[test]
    fn unbridge_leaves_what_is_not_this_pairs() {
        let p = pair("others");
        // Another phone's forward on the pair's port, and a watch reverse to another port.
        p.rig.adb("emulator-5558").bind_forward(P0, 5601, false).expect("another phone's");
        p.watch.reverse(5601, 5700).expect("another bridge's reverse");
        // And a forward of the phone's own to something that isn't the companion.
        p.phone.bind_forward(P0 + 1, 5277, false).expect("head unit");
        let done = unbridge(Some(&p.phone), Some(&p.watch), P0, LEGACY_COMPANION).expect("unbridge");
        assert!(done.forwards_removed.is_empty() && !done.reverse_removed, "{done:?}");
        assert_eq!(p.rig.fake.forwards().len(), 2);
        assert_eq!(p.rig.fake.reverses(), [format!("{WATCH} tcp:5601 tcp:5700")]);
    }

    #[test]
    fn unbridge_checks_its_input_before_adb() {
        let p = pair("badinput");
        assert!(unbridge(Some(&p.phone), Some(&p.watch), P0, "com.x;reboot").is_err());
        assert!(unbridge(Some(&p.phone), Some(&p.watch), 0, PIXEL_COMPANION).is_err());
        assert!(p.calls().is_empty());
    }
}
