//! A running device's network, over its console and adb: the NAT's shaping (`network
//! speed|delay|status`, which simee's emulator and the stock one answer alike, and simee's own
//! `network loss`, which the stock console doesn't have), airplane mode
//! (what `network_shape`'s offline is on Android), the guest's open sockets with the process
//! holding each, and the bytes each app uid moved.
//!
//! Sockets are the kernel's tables (`/proc/net/{tcp,tcp6,udp,udp6}`), matched to processes by the
//! socket inodes in `/proc/<pid>/fd`: as root (`su 0`, which google_apis images have) that is
//! every process; as the shell user only its own, and a socket nobody can be matched to is named
//! by its owner's uid. Traffic is the kernel's per-uid counters since boot: the eBPF
//! `mAppUidStatsMap` that `dumpsys` prints (Android 10 on), else `/proc/net/xt_qtaguid/stats`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use sim_core::network::Conditions;

use crate::Device;
use crate::apps::{Adb, failure};

/// One internet socket inside the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Socket {
    /// The process holding it, `None` when no process could be matched to it (without root, any
    /// other user's; with it, one closed between the two reads, or the kernel's own).
    pub pid: Option<i32>,
    pub fd: Option<i32>,
    /// The socket's owner.
    pub uid: u32,
    /// The holding process's name, else the owner's package or user ([`uid_name`]).
    pub process: String,
    /// The IP protocol number: 6 (TCP) or 17 (UDP).
    pub protocol: i32,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    /// A TCP socket's state, numbered as macOS's `TSI_S_*` are (what `sim_debug` names): see
    /// [`tsi_state`]. `None` for UDP.
    pub tcp_state: Option<i32>,
}

/// What [`Adb::sockets`] found, and whether it could look at every process (as root).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sockets {
    pub sockets: Vec<Socket>,
    pub as_root: bool,
}

/// Bytes one uid moved since boot, on every interface but loopback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UidTraffic {
    pub uid: u32,
    /// The app's own process (the one named for its package) when it runs.
    pub pid: Option<i32>,
    /// The uid's package, else its user ([`uid_name`]).
    pub process: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// The line before each part of one adb call's output.
const MARK: &str = "@net:";

/// The kernel's socket tables, the socket fds of every process it may read, the process table
/// (toybox `ps -A -o PID,UID,USER,NAME`, [`parse_ps`]) and
/// each package's uid, in one shell, as root when `su 0` works.
const SOCKETS: &str = "s='for f in tcp tcp6 udp udp6; do echo @net:$f; cat /proc/net/$f 2>&1; done; \
echo @net:fd; ls -l /proc/[0-9]*/fd 2>/dev/null; echo @net:ps; ps -A -o PID,UID,USER,NAME; \
echo @net:packages; pm list packages -U'; \
if su 0 true 2>/dev/null; then echo @net:root; su 0 sh -c \"$s\"; else sh -c \"$s\"; fi";

/// The per-uid counters: xt_qtaguid where the kernel still has it (Android 9 and older), else the
/// first `dumpsys` that prints the eBPF map (connectivity's from Android 12, netstats', netd's on
/// 10 and 11); then the process table and each package's uid.
const TRAFFIC: &str = "if [ -r /proc/net/xt_qtaguid/stats ]; then echo @net:qtaguid; cat /proc/net/xt_qtaguid/stats; \
else echo @net:bpf; for c in 'connectivity trafficcontroller' netstats 'netd trafficcontroller'; do \
o=$(dumpsys $c 2>/dev/null); case \"$o\" in *mAppUidStatsMap*) echo \"$o\"; break;; esac; done; fi; \
echo @net:ps; ps -A -o PID,UID,USER,NAME; echo @net:packages; pm list packages -U";

/// The TCP tables alone: what [`Adb::listening`] reads.
const TCP_TABLES: &str = "echo @net:tcp; cat /proc/net/tcp; echo @net:tcp6; cat /proc/net/tcp6";

impl Device {
    /// What the emulator's NAT is shaped to (the console's `network status`), with the report's
    /// lines as it gave them.
    pub fn network_conditions(&self) -> Result<(Conditions, Vec<String>), String> {
        let lines = self.console(&["network status".to_owned()])?;
        let conditions =
            Conditions::from_status(&lines.join("\n")).ok_or_else(|| format!("`network status` answered: {}", lines.join(" | ")))?;
        Ok((conditions, lines))
    }

    /// Sets the NAT's `network speed` and `network delay` (stock's syntax: a preset name,
    /// `<up>:<down>` kbit/s, `<min>:<max>` ms) and simee's `network loss` (`<percent>`,
    /// `<up>:<down>` or `none`), those given, in one console session; returns the commands sent.
    /// Loss on the stock emulator is refused before anything is sent ([`check_network_loss`](Self::check_network_loss)).
    pub fn set_network(&self, speed: Option<&str>, delay: Option<&str>, loss: Option<&str>) -> Result<Vec<String>, String> {
        if loss.is_some() {
            self.check_network_loss()?;
        }
        let commands: Vec<String> = speed
            .map(|s| format!("network speed {s}"))
            .into_iter()
            .chain(delay.map(|d| format!("network delay {d}")))
            .chain(loss.map(|l| format!("network loss {l}")))
            .collect();
        if !commands.is_empty() {
            self.console(&commands)?;
        }
        Ok(commands)
    }

    /// Whether the running device can lose packets: only under simee's emu, whose NAT drops them
    /// (`network loss`). The stock emulator's console has no loss command, so a device it runs
    /// gets the reason and what to boot instead.
    pub fn check_network_loss(&self) -> Result<(), String> {
        self.running_simee("packet loss (the stock emulator's console has no `network loss`)").map(|_| ())
    }
}

impl Adb {
    /// Whether airplane mode is on (`settings get global airplane_mode_on`).
    pub fn airplane_mode(&self) -> Result<bool, String> {
        let out = self.shell("settings get global airplane_mode_on", self.deadlines.query)?;
        match out.trim() {
            "1" => Ok(true),
            "0" | "null" => Ok(false),
            other => Err(format!("airplane_mode_on is `{other}`")),
        }
    }

    /// The guest's API level (`getprop ro.build.version.sdk`).
    pub fn api_level(&self) -> Result<u32, String> {
        let out = self.shell("getprop ro.build.version.sdk", self.deadlines.query)?;
        out.trim().parse().map_err(|_| format!("ro.build.version.sdk is `{}`", out.trim()))
    }

    /// Turns airplane mode on or off, which takes every network down or brings them back as a
    /// user's toggle does: `cmd connectivity airplane-mode` from Android 11 (API 30), else the
    /// setting and the broadcast that applies it. Returns the commands run.
    pub fn set_airplane_mode(&self, on: bool) -> Result<Vec<String>, String> {
        if self.api_level()? >= 30 {
            let command = format!("cmd connectivity airplane-mode {}", if on { "enable" } else { "disable" });
            let out = self.shell(&command, self.deadlines.query)?;
            // `cmd` exits 0 even for a subcommand it doesn't have.
            if out.contains("Unknown command") || out.contains("No shell command implementation") {
                return Err(failure(&command, &out));
            }
            return Ok(vec![command]);
        }
        let commands = vec![
            format!("settings put global airplane_mode_on {}", u8::from(on)),
            format!("am broadcast -a android.intent.action.AIRPLANE_MODE --ez state {on}"),
        ];
        for command in &commands {
            self.shell(command, self.deadlines.query)?;
        }
        Ok(commands)
    }

    /// The device's internet sockets, by process then fd, those no process could be matched to
    /// last: one adb call.
    pub fn sockets(&self) -> Result<Sockets, String> {
        sockets_from(&self.shell(SOCKETS, self.deadlines.query)?)
    }

    /// Bytes each uid moved since boot, busiest first (uids that moved none are left out): one
    /// adb call.
    pub fn traffic(&self) -> Result<Vec<UidTraffic>, String> {
        traffic_from(&self.shell(TRAFFIC, self.deadlines.query)?)
    }
}

impl Adb {
    /// `adb forward tcp:0 tcp:<port>`: forwards a host port adb picks (free at the time) to `port`
    /// in the device, and returns it. The adb server holds the forward, so it outlives simee, and
    /// goes with the device's next boot or the server's restart.
    pub fn forward(&self, port: u16) -> Result<u16, String> {
        let guest = format!("tcp:{port}");
        match self.run(&["forward", "tcp:0", &guest], self.deadlines.query)? {
            (true, text) => text
                .split_whitespace()
                .find_map(|w| w.parse::<u16>().ok())
                .ok_or_else(|| format!("adb forward tcp:0 {guest} printed no host port: {}", text.trim())),
            (false, text) => Err(failure(&format!("adb forward tcp:0 {guest}"), &text)),
        }
    }

    /// `adb forward --remove tcp:<host_port>`.
    pub fn remove_forward(&self, host_port: u16) -> Result<(), String> {
        let host = format!("tcp:{host_port}");
        match self.run(&["forward", "--remove", &host], self.deadlines.query)? {
            (true, _) => Ok(()),
            (false, text) => Err(failure(&format!("adb forward --remove {host}"), &text)),
        }
    }

    /// This device's TCP forwards, (host port, device port) each: `adb forward --list`, which
    /// lists every device's.
    pub fn forwards(&self) -> Result<Vec<(u16, u16)>, String> {
        match self.run(&["forward", "--list"], self.deadlines.query)? {
            (true, text) => Ok(forwards_of(&text, &self.serial)),
            (false, text) => Err(failure("adb forward --list", &text)),
        }
    }

    /// The TCP ports something in the device listens on (IPv4 or IPv6, any address), ascending:
    /// the kernel's tables, which any user may read.
    pub fn listening(&self) -> Result<Vec<u16>, String> {
        Ok(listening_in(&self.shell(TCP_TABLES, self.deadlines.query)?))
    }
}

/// `serial`'s TCP forwards in `adb forward --list`'s lines (`<serial> tcp:<host> tcp:<device>`).
fn forwards_of(text: &str, serial: &str) -> Vec<(u16, u16)> {
    let tcp = |s: &str| s.strip_prefix("tcp:").and_then(|p| p.parse::<u16>().ok());
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            match f[..] {
                [s, host, guest] if s == serial => Some((tcp(host)?, tcp(guest)?)),
                _ => None,
            }
        })
        .collect()
}

/// The listening ports in [`TCP_TABLES`]' output (Linux's `TCP_LISTEN` is 10).
fn listening_in(text: &str) -> Vec<u16> {
    let parts = parts(text);
    let mut ports: Vec<u16> = [("tcp", false), ("tcp6", true)]
        .into_iter()
        .flat_map(|(table, v6)| parse_proc_net(parts.get(table).map_or("", String::as_str), v6))
        .filter(|e| e.state == 10)
        .map(|e| e.local.port())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// One row of a `/proc/net/{tcp,udp}[6]` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    local: SocketAddr,
    remote: SocketAddr,
    /// Linux's `TCP_*` state (UDP's are 1 or 7 and mean nothing here).
    state: u8,
    uid: u32,
    /// 0 for a socket no file holds (TIME_WAIT).
    inode: u64,
}

/// The rows of one kernel socket table; `v6` for `tcp6`/`udp6`. A row that doesn't parse is
/// skipped, the header with it.
fn parse_proc_net(text: &str, v6: bool) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || !f[0].ends_with(':') {
                return None;
            }
            Some(Entry {
                local: address(f[1], v6)?,
                remote: address(f[2], v6)?,
                state: u8::from_str_radix(f[3], 16).ok()?,
                uid: f[7].parse().ok()?,
                inode: f[9].parse().ok()?,
            })
        })
        .collect()
}

/// `0100007F:1F90` (IPv4) or 32 hex digits and a port (IPv6): the kernel prints each 32-bit word
/// of the address as the guest's (little-endian) integer, and the port in host order.
fn address(text: &str, v6: bool) -> Option<SocketAddr> {
    let (addr, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| addr.get(i * 8..i * 8 + 8).and_then(|w| u32::from_str_radix(w, 16).ok()).map(u32::to_le_bytes);
    if v6 {
        if addr.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for i in 0..4 {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
        }
        Some(SocketAddr::new(Ipv6Addr::from(bytes).into(), port))
    } else {
        (addr.len() == 8).then(|| word(0)).flatten().map(|b| SocketAddr::new(Ipv4Addr::from(b).into(), port))
    }
}

/// Linux's TCP state (`include/net/tcp_states.h`, 1 to 11) as macOS's `TSI_S_*` number the same
/// state, the numbering `sim_debug` gives `lsof`'s names.
pub fn tsi_state(linux: u8) -> Option<i32> {
    Some(match linux {
        1 => 4,  // ESTABLISHED
        2 => 2,  // SYN_SENT
        3 => 3,  // SYN_RECV: SYN_RCVD
        4 => 6,  // FIN_WAIT1
        5 => 9,  // FIN_WAIT2
        6 => 10, // TIME_WAIT
        7 => 0,  // CLOSE: CLOSED
        8 => 5,  // CLOSE_WAIT
        9 => 8,  // LAST_ACK
        10 => 1, // LISTEN
        11 => 7, // CLOSING
        _ => return None,
    })
}

/// Socket inodes to the (pid, fd)s holding them, from toybox `ls -l /proc/<pid>/fd ...`: a
/// `/proc/<pid>/fd:` header per process, then `... <fd> -> socket:[<inode>]` lines.
fn parse_fds(text: &str) -> HashMap<u64, Vec<(i32, i32)>> {
    let mut map: HashMap<u64, Vec<(i32, i32)>> = HashMap::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(dir) = line.strip_prefix("/proc/").and_then(|l| l.strip_suffix("/fd:")) {
            pid = dir.parse().ok();
            continue;
        }
        let (Some(pid), Some((left, target))) = (pid, line.split_once(" -> ")) else { continue };
        let inode = target.trim().strip_prefix("socket:[").and_then(|t| t.strip_suffix(']')).and_then(|t| t.parse().ok());
        let fd = left.split_whitespace().last().and_then(|f| f.parse().ok());
        if let (Some(inode), Some(fd)) = (inode, fd) {
            map.entry(inode).or_default().push((pid, fd));
        }
    }
    for holders in map.values_mut() {
        holders.sort_unstable();
    }
    map
}

/// One row of `ps -A -o PID,UID,USER,NAME`.
struct Proc {
    pid: i32,
    uid: u32,
    user: String,
    name: String,
}

fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (pid, uid, user) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?.to_owned());
            let name = f.collect::<Vec<_>>().join(" ");
            Some(Proc { pid, uid, user, name })
        })
        .collect()
}

/// Each uid's packages, from `pm list packages -U` (`package:<name> uid:<uid>[,<uid>...]`).
fn parse_package_uids(text: &str) -> HashMap<u32, Vec<String>> {
    let mut map: HashMap<u32, Vec<String>> = HashMap::new();
    for line in text.lines() {
        let Some((name, uids)) = line.trim().strip_prefix("package:").and_then(|l| l.split_once(" uid:")) else { continue };
        for uid in uids.split(',').filter_map(|u| u.trim().parse().ok()) {
            map.entry(uid).or_default().push(name.to_owned());
        }
    }
    map
}

/// What names a uid: its package when it has exactly one, else its user as `ps` shows it
/// (`system`, `u0_a86`) when one of its processes runs, else `uid <n>`.
fn uid_name(uid: u32, procs: &[Proc], packages: &HashMap<u32, Vec<String>>) -> String {
    match packages.get(&uid).map(Vec::as_slice) {
        Some([only]) => only.clone(),
        _ => procs.iter().find(|p| p.uid == uid).map_or_else(|| format!("uid {uid}"), |p| p.user.clone()),
    }
}

/// The parts of a marked output ([`MARK`]), by name.
fn parts(out: &str) -> HashMap<&str, String> {
    let mut parts: HashMap<&str, String> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in out.lines() {
        if let Some(name) = line.trim_end().strip_prefix(MARK) {
            current = Some(name);
            parts.entry(name).or_default();
        } else if let Some(name) = current {
            let part = parts.entry(name).or_default();
            part.push_str(line);
            part.push('\n');
        }
    }
    parts
}

/// The sockets in [`SOCKETS`]'s output.
fn sockets_from(out: &str) -> Result<Sockets, String> {
    let parts = parts(out);
    let tables = [("tcp", 6, false), ("tcp6", 6, true), ("udp", 17, false), ("udp6", 17, true)];
    // `cat`'s own error (permission denied) is kept in the part, so a table the shell couldn't read fails here.
    let tcp_readable = parts.get("tcp").is_some_and(|t| t.contains("local_address"));
    if !tcp_readable || !parts.contains_key("ps") {
        return Err(failure("reading /proc/net", parts.get("tcp").filter(|t| !t.trim().is_empty()).map_or(out, String::as_str)));
    }
    let part = |name: &str| parts.get(name).map_or("", String::as_str);
    let fds = parse_fds(part("fd"));
    let procs = parse_ps(part("ps"));
    let packages = parse_package_uids(part("packages"));
    let name_of = |pid: i32| procs.iter().find(|p| p.pid == pid).map(|p| p.name.clone());
    let (mut held, mut loose) = (Vec::new(), Vec::new());
    for (table, protocol, v6) in tables {
        for e in parse_proc_net(part(table), v6).into_iter().filter(|e| e.inode != 0) {
            let tcp_state = if protocol == 6 { tsi_state(e.state) } else { None };
            let socket = |pid: Option<i32>, fd: Option<i32>, process: String| Socket {
                pid,
                fd,
                uid: e.uid,
                process,
                protocol,
                local: e.local,
                remote: e.remote,
                tcp_state,
            };
            match fds.get(&e.inode) {
                Some(holders) => held.extend(holders.iter().map(|&(pid, fd)| {
                    let process = name_of(pid).unwrap_or_else(|| uid_name(e.uid, &procs, &packages));
                    socket(Some(pid), Some(fd), process)
                })),
                None => loose.push(socket(None, None, uid_name(e.uid, &procs, &packages))),
            }
        }
    }
    held.sort_by_key(|s| (s.pid, s.fd));
    held.extend(loose);
    Ok(Sockets { sockets: held, as_root: parts.contains_key("root") })
}

/// Received and sent bytes per uid in `/proc/net/xt_qtaguid/stats`: its untagged rows (tag `0x0`;
/// a tagged row counts a subset of them again), both counter sets, every interface but `lo`.
fn parse_qtaguid(text: &str) -> HashMap<u32, (u64, u64)> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else { return HashMap::new() };
    let col = |name: &str| header.split_whitespace().position(|c| c == name);
    let (Some(iface), Some(tag), Some(uid), Some(rx), Some(tx)) =
        (col("iface"), col("acct_tag_hex"), col("uid_tag_int"), col("rx_bytes"), col("tx_bytes"))
    else {
        return HashMap::new();
    };
    let mut map: HashMap<u32, (u64, u64)> = HashMap::new();
    for line in lines {
        let f: Vec<&str> = line.split_whitespace().collect();
        let get = |i: usize| f.get(i).copied();
        if get(iface) == Some("lo") || get(tag) != Some("0x0") {
            continue;
        }
        let (Some(uid), Some(rx), Some(tx)) = (
            get(uid).and_then(|u| u.parse().ok()),
            get(rx).and_then(|r| r.parse::<u64>().ok()),
            get(tx).and_then(|t| t.parse::<u64>().ok()),
        ) else {
            continue;
        };
        let e = map.entry(uid).or_default();
        *e = (e.0 + rx, e.1 + tx);
    }
    map
}

/// Received and sent bytes per uid in the eBPF `mAppUidStatsMap` a `dumpsys` printed: the rows
/// of five numbers (`uid rxBytes rxPackets txBytes txPackets`) after its name.
fn parse_uid_stats_map(text: &str) -> HashMap<u32, (u64, u64)> {
    let mut map = HashMap::new();
    // Dumps print a status block first (`mAppUidStatsMap: OK`, older netd's `mAppUidStatsMap status: OK`),
    // so the rows start after the column header, which only the contents carry.
    let mut lines = text.lines().map(str::trim).skip_while(|l| !l.starts_with("uid rxBytes")).skip(1);
    for line in lines.by_ref() {
        if line.starts_with("uid ") || line.is_empty() {
            continue;
        }
        let n: Vec<u64> = line.split_whitespace().map_while(|w| w.parse().ok()).collect();
        let (Some(&uid), true) = (n.first(), n.len() == 5) else { break };
        let Ok(uid) = u32::try_from(uid) else { break };
        map.insert(uid, (n[1], n[3]));
    }
    map
}

/// The traffic in [`TRAFFIC`]'s output.
fn traffic_from(out: &str) -> Result<Vec<UidTraffic>, String> {
    let parts = parts(out);
    let stats = match (parts.get("qtaguid"), parts.get("bpf")) {
        (Some(q), _) => parse_qtaguid(q),
        (None, Some(b)) => parse_uid_stats_map(b),
        (None, None) => HashMap::new(),
    };
    if stats.is_empty() {
        return Err("the device reports no per-uid traffic (neither /proc/net/xt_qtaguid/stats nor dumpsys's mAppUidStatsMap)".into());
    }
    let procs = parse_ps(parts.get("ps").map_or("", String::as_str));
    let packages = parse_package_uids(parts.get("packages").map_or("", String::as_str));
    let mut rows: Vec<UidTraffic> = stats
        .into_iter()
        .filter(|(_, (rx, tx))| rx + tx > 0)
        .map(|(uid, (bytes_in, bytes_out))| {
            let process = uid_name(uid, &procs, &packages);
            let own = packages.get(&uid).is_some_and(|p| p.contains(&process));
            let pid = own.then(|| procs.iter().find(|p| p.uid == uid && p.name == process).map(|p| p.pid)).flatten();
            UidTraffic { uid, pid, process, bytes_in, bytes_out }
        })
        .collect();
    rows.sort_by_key(|r| (std::cmp::Reverse(r.bytes_in + r.bytes_out), r.uid));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    #[test]
    fn forwards_are_read_for_one_serial_and_listening_ports_from_the_tcp_tables() {
        let list = "emulator-5554 tcp:41001 tcp:8080\nemulator-5556 tcp:41002 tcp:9000\nemulator-5554 localabstract:x tcp:1\n\
                    emulator-5554 tcp:41003 tcp:9443\n";
        assert_eq!(forwards_of(list, "emulator-5554"), [(41001, 8080), (41003, 9443)]);
        assert_eq!(forwards_of(list, "emulator-5558"), []);
        let tables = "@net:tcp\n  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1388 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 17 1 0 100 0 0 10 0\n\
   1: 0F02000A:A1B2 2E2E3AD8:01BB 01 00000000:00000000 02:000A7B2C 00000000 10086        0 18 2 0 20 4 30 10 -1\n\
@net:tcp6\n  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 19 1 0 100 0 0 10 0\n\
   1: 00000000000000000000000000000000:1388 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 20 1 0 100 0 0 10 0\n";
        assert_eq!(listening_in(tables), [5000, 8080], "LISTEN rows only, IPv4 and IPv6, once each");
        assert_eq!(listening_in(""), Vec::<u16>::new());
    }

    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:13AD 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 18001 1 0000000000000000 100 0 0 10 0
   1: 0F02000A:A1B2 2E2E3AD8:01BB 01 00000000:00000000 02:000A7B2C 00000000 10086        0 18002 2 0000000000000000 20 4 30 10 -1
   2: 0F02000A:A1B4 2E2E3AD8:01BB 06 00000000:00000000 03:00001770 00000000     0        0 0 3 0000000000000000
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 18003 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000F02000A:B000 0000000000000000FFFF00002E2E3AD8:01BB 01 00000000:00000000 02:000A7B2C 00000000 10086        0 18004 2 0000000000000000 20 4 30 10 -1
";
    const UDP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  100: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000  1021        0 18005 2 0000000000000000 0
";
    const FDS: &str = "/proc/1/fd:
total 0
lrwx------ 1 root root 64 2026-11-28 10:00 0 -> /dev/null
lrwx------ 1 root root 64 2026-11-28 10:00 4 -> socket:[9999]

/proc/612/fd:
total 0
lrwx------ 1 system system 64 2026-11-28 10:00 300 -> socket:[18003]

/proc/4321/fd:
total 0
lr-x------ 1 u0_a86 u0_a86 64 2026-11-28 10:00 3 -> /apex/com.android.art/javalib/core-oj.jar
lrwx------ 1 u0_a86 u0_a86 64 2026-11-28 10:00 58 -> socket:[18004]
lrwx------ 1 u0_a86 u0_a86 64 2026-11-28 10:00 57 -> socket:[18002]
";
    const PS: &str = "  PID   UID USER           NAME
    1     0 root           init
  612  1000 system         system_server
 4321 10086 u0_a86         com.example.app
 4400 10086 u0_a86         com.example.app:remote
";
    const PACKAGES: &str = "package:com.example.app uid:10086
package:android uid:1000
package:com.android.settings uid:1000
";

    fn v4(s: &str) -> SocketAddr {
        s.parse().expect("address")
    }

    #[test]
    fn proc_net_lines_read_as_little_endian_addresses() {
        let tcp = parse_proc_net(TCP, false);
        assert_eq!(tcp.len(), 3);
        assert_eq!(
            (tcp[0].local, tcp[0].remote, tcp[0].state, tcp[0].uid, tcp[0].inode),
            (v4("0.0.0.0:5037"), v4("0.0.0.0:0"), 0x0A, 0, 18001)
        );
        assert_eq!((tcp[1].local, tcp[1].remote, tcp[1].state, tcp[1].uid), (v4("10.0.2.15:41394"), v4("216.58.46.46:443"), 0x01, 10086));
        assert_eq!(tcp[2].inode, 0, "a TIME_WAIT socket belongs to no one");
        let tcp6 = parse_proc_net(TCP6, true);
        assert_eq!(tcp6[0].local, "[::]:8080".parse::<SocketAddr>().expect("v6"));
        assert_eq!(tcp6[1].local, "[::ffff:10.0.2.15]:45056".parse::<SocketAddr>().expect("mapped"));
        assert_eq!(tcp6[1].remote, "[::ffff:216.58.46.46]:443".parse::<SocketAddr>().expect("mapped"));
        let udp = parse_proc_net(UDP, false);
        assert_eq!((udp[0].local, udp[0].uid, udp[0].inode), (v4("0.0.0.0:5353"), 1021, 18005));
        assert!(parse_proc_net("garbage\n   0: zz:1 00:0 0A", false).is_empty(), "a line that doesn't parse is skipped");
    }

    #[test]
    fn linux_tcp_states_take_the_names_lsof_gives_them() {
        // Linux's TCP_* (include/net/tcp_states.h) to the TSI_S_* order sim-debug names.
        let names = [
            "",
            "ESTABLISHED",
            "SYN_SENT",
            "SYN_RCVD",
            "FIN_WAIT_1",
            "FIN_WAIT_2",
            "TIME_WAIT",
            "CLOSED",
            "CLOSE_WAIT",
            "LAST_ACK",
            "LISTEN",
            "CLOSING",
        ];
        const TSI: [&str; 11] = [
            "CLOSED",
            "LISTEN",
            "SYN_SENT",
            "SYN_RCVD",
            "ESTABLISHED",
            "CLOSE_WAIT",
            "FIN_WAIT_1",
            "CLOSING",
            "LAST_ACK",
            "FIN_WAIT_2",
            "TIME_WAIT",
        ];
        for (linux, name) in names.iter().enumerate().skip(1) {
            let tsi = tsi_state(linux as u8).expect("a state");
            assert_eq!(TSI[tsi as usize], *name, "linux state {linux}");
        }
        assert_eq!(tsi_state(0), None);
        assert_eq!(tsi_state(12), None);
    }

    #[test]
    fn socket_inodes_map_to_the_processes_holding_them() {
        let fds = parse_fds(FDS);
        assert_eq!(fds.get(&18002), Some(&vec![(4321, 57)]));
        assert_eq!(fds.get(&18003), Some(&vec![(612, 300)]));
        assert_eq!(fds.get(&9999), Some(&vec![(1, 4)]));
        assert_eq!(fds.len(), 4, "only sockets: {fds:?}");
    }

    fn listing(root: bool) -> String {
        let fds = if root { FDS } else { "" };
        let root = if root { "@net:root\n" } else { "" };
        format!("{root}@net:tcp\n{TCP}@net:tcp6\n{TCP6}@net:udp\n{UDP}@net:udp6\n@net:fd\n{fds}@net:ps\n{PS}@net:packages\n{PACKAGES}")
    }

    #[test]
    fn sockets_are_listed_by_pid_and_fd_with_their_owner() {
        let found = sockets_from(&listing(true)).expect("sockets");
        assert!(found.as_root);
        type Row<'a> = (Option<i32>, Option<i32>, &'a str, i32, Option<i32>);
        let rows: Vec<Row> = found.sockets.iter().map(|s| (s.pid, s.fd, s.process.as_str(), s.protocol, s.tcp_state)).collect();
        assert_eq!(
            rows,
            [
                (Some(612), Some(300), "system_server", 6, Some(1)),
                (Some(4321), Some(57), "com.example.app", 6, Some(4)),
                (Some(4321), Some(58), "com.example.app", 6, Some(4)),
                // No process holds these (they were closed, or belong to the kernel): their owner's uid names them.
                (None, None, "root", 6, Some(1)),
                (None, None, "uid 1021", 17, None),
            ]
        );
        assert_eq!(found.sockets[1].remote, v4("216.58.46.46:443"));
        assert_eq!(found.sockets[3].uid, 0);
    }

    #[test]
    fn without_root_each_socket_is_named_by_its_uid() {
        let found = sockets_from(&listing(false)).expect("sockets");
        assert!(!found.as_root);
        let owners: Vec<(Option<i32>, u32, &str)> = found.sockets.iter().map(|s| (s.pid, s.uid, s.process.as_str())).collect();
        // The uid's one package, else its user as `ps` names it, else the number.
        assert_eq!(
            owners,
            [
                (None, 0, "root"),
                (None, 10086, "com.example.app"),
                (None, 1000, "system"),
                (None, 10086, "com.example.app"),
                (None, 1021, "uid 1021")
            ]
        );
    }

    #[test]
    fn a_listing_without_the_kernels_tables_is_an_error() {
        let err =
            sockets_from("@net:tcp\n/system/bin/sh: cat: /proc/net/tcp: Permission denied\n@net:tcp6\n@net:ps\n").expect_err("no tables");
        assert!(err.contains("Permission denied"), "{err}");
    }

    const QTAGUID: &str = "idx iface acct_tag_hex uid_tag_int cnt_set rx_bytes rx_packets tx_bytes tx_packets rx_tcp_bytes rx_tcp_packets
2 wlan0 0x0 0 0 100 1 200 2 0 0
3 wlan0 0x0 10086 0 1000 10 300 3 0 0
4 wlan0 0x0 10086 1 500 5 100 1 0 0
5 wlan0 0x2a00000000 10086 0 900 9 250 2 0 0
6 lo 0x0 10086 0 7777 7 7777 7 0 0
7 radio0 0x0 1000 0 40 1 60 1 0 0
";

    #[test]
    fn qtaguid_counts_untagged_rows_off_loopback() {
        let stats = parse_qtaguid(QTAGUID);
        assert_eq!(stats.get(&10086), Some(&(1500, 400)), "both counter sets, not the tagged subset or lo");
        assert_eq!(stats.get(&1000), Some(&(40, 60)));
        assert_eq!(stats.get(&0), Some(&(100, 200)));
    }

    /// The eBPF map as `dumpsys` prints it (netd's TrafficController before Android 13, the
    /// connectivity module's after): a header line, then `uid rxBytes rxPackets txBytes txPackets`.
    /// Source: AOSP packages/modules/Connectivity, service-t/.../NetworkStatsService.java
    /// (`dumpMapStatus` -> `BpfDump.dumpMapStatus` prints `<map>: OK` under "BPF map status:";
    /// `dumpAppUidStatsMapLocked` -> `BpfDump.dumpMap` prints `<map>:`, the header, then the rows
    /// under "BPF map content:"). Confirmed from those sources; netd's older `status: OK` spelling is inferred.
    const BPF: &str = "BPF map status:
  mCookieTagMap: OK
  mAppUidStatsMap: OK
  mStatsMapA status: OK
BPF map content:
TrafficController:
  mCookieTagMap:
    cookie=1 tag=0x0 uid=10086
  mAppUidStatsMap:
    uid rxBytes rxPackets txBytes txPackets
    1000 4096 10 2048 8
    10086 123456 100 23456 50
    10090 0 0 0 0
  mStatsMapA:
    ifaceIndex ifaceName tag_hex uid_int cnt_set rxBytes rxPackets txBytes txPackets
    2 wlan0 0x0 10086 0 99 1 99 1
";

    #[test]
    fn the_uid_stats_map_is_read_from_dumpsys() {
        let stats = parse_uid_stats_map(BPF);
        assert_eq!(stats.len(), 3, "only mAppUidStatsMap's rows: {stats:?}");
        assert_eq!(stats.get(&10086), Some(&(123_456, 23_456)));
        assert_eq!(stats.get(&1000), Some(&(4096, 2048)));
    }

    #[test]
    fn traffic_is_per_uid_busiest_first_with_its_process() {
        let out = format!("@net:bpf\n{BPF}@net:ps\n{PS}@net:packages\n{PACKAGES}");
        let rows = traffic_from(&out).expect("traffic");
        let got: Vec<(u32, Option<i32>, &str, u64, u64)> =
            rows.iter().map(|r| (r.uid, r.pid, r.process.as_str(), r.bytes_in, r.bytes_out)).collect();
        // The pid is the app's own process (named for its package); a shared uid has none.
        assert_eq!(got, [(10086, Some(4321), "com.example.app", 123_456, 23_456), (1000, None, "system", 4096, 2048)]);
        let old = format!("@net:qtaguid\n{QTAGUID}@net:ps\n{PS}@net:packages\n{PACKAGES}");
        let rows = traffic_from(&old).expect("qtaguid");
        assert_eq!((rows[0].uid, rows[0].bytes_in, rows[0].bytes_out), (10086, 1500, 400));
        assert_eq!((rows[1].uid, rows[1].pid, rows[1].process.as_str()), (0, None, "root"));
        let err = traffic_from(&format!("@net:ps\n{PS}")).expect_err("no counters");
        assert!(err.contains("per-uid"), "{err}");
    }
}

/// Run against `testing`'s fake adb and emulator, which are unix only.
#[cfg(all(test, unix))]
mod device_tests {
    use crate::testing::{fake_adb, fake_emulator, fake_stock_emulator};

    fn device(tag: &str) -> (crate::Android, std::path::PathBuf, crate::testing::FakeAdb, crate::testing::FakeEmulator) {
        device_as(tag, false)
    }

    /// A booted AVD under simee's emu, or under the stock emulator (`stock`).
    fn device_as(tag: &str, stock: bool) -> (crate::Android, std::path::PathBuf, crate::testing::FakeAdb, crate::testing::FakeEmulator) {
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-net-{tag}-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/x86_64");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = crate::Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;x86_64", None).expect("create");
        let adb = fake_adb(&android);
        let emulator = if stock { fake_stock_emulator(&android, "phone") } else { fake_emulator(&android, "phone") };
        (android, root, adb, emulator)
    }

    #[test]
    fn forwards_are_added_listed_and_removed_through_adb() {
        let (android, root, adb, _emulator) = device("forward");
        let a = android.device("phone").expect("device").adb(&android).expect("adb");
        assert_eq!(a.forward(8080), Ok(41001));
        assert_eq!(a.forward(9000), Ok(41002));
        assert_eq!(a.forwards(), Ok(vec![(41001, 8080), (41002, 9000)]));
        a.remove_forward(41001).expect("remove");
        assert_eq!(a.forwards(), Ok(vec![(41002, 9000)]));
        assert_eq!(a.listening(), Ok(vec![8080]));
        let calls = adb.calls();
        assert!(
            calls.contains(&"forward tcp:0 tcp:8080".to_owned()) && calls.contains(&"forward --remove tcp:41001".to_owned()),
            "{calls:?}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn shaping_reads_back_through_the_console() {
        let (android, root, _adb, emulator) = device("console");
        let d = android.device("phone").expect("device");
        let (unshaped, lines) = d.network_conditions().expect("status");
        assert!(!unshaped.is_shaped());
        assert_eq!(lines[0], "Current network status:");
        let sent = d.set_network(Some("330:780"), Some("200"), None).expect("shape");
        assert_eq!(sent, ["network speed 330:780", "network delay 200"]);
        let (c, _) = d.network_conditions().expect("status");
        assert_eq!((c.upload_bps, c.download_bps, c.min_latency_ms, c.max_latency_ms), (330_000.0, 780_000.0, 200, 200));
        assert_eq!(d.set_network(None, None, None).expect("nothing"), Vec::<String>::new());
        let err = d.set_network(Some("warp"), None, None).expect_err("bad speed");
        assert!(err.contains("invalid <speed>"), "{err}");
        let console = emulator.console.lock().expect("console").clone();
        assert_eq!(console.iter().filter(|c| c.starts_with("network speed") || c.starts_with("network delay")).count(), 3);
        std::fs::remove_dir_all(root).ok();
    }

    /// simee's `network loss`, sent with the rest and read back from `network status`.
    #[test]
    fn loss_is_set_and_read_back_through_simees_console() {
        let (android, root, _adb, emulator) = device("loss");
        let d = android.device("phone").expect("device");
        assert_eq!(d.check_network_loss(), Ok(()));
        let sent = d.set_network(None, Some("100"), Some("10")).expect("loss");
        assert_eq!(sent, ["network delay 100", "network loss 10"]);
        let (c, lines) = d.network_conditions().expect("status");
        assert_eq!((c.upload_loss_pct, c.download_loss_pct), (10.0, 10.0));
        assert!(lines.iter().any(|l| l.trim() == "upload loss:      10 %"), "{lines:?}");
        assert_eq!(d.set_network(None, None, Some("none")).expect("clear"), ["network loss none"]);
        let (c, lines) = d.network_conditions().expect("status");
        assert!(!c.is_lossy() && c.min_latency_ms == 100, "only the loss changed: {c:?}");
        assert!(!lines.iter().any(|l| l.contains("loss")), "{lines:?}");
        let err = d.set_network(None, None, Some("lots")).expect_err("bad loss");
        assert!(err.contains("invalid <loss>"), "{err}");
        assert_eq!(emulator.console.lock().expect("console").iter().filter(|c| c.starts_with("network loss")).count(), 3);
        std::fs::remove_dir_all(root).ok();
    }

    /// The stock emulator's console has no loss command: asking for loss there is refused before
    /// anything is sent, saying why and what to boot instead; speed and delay still work.
    #[test]
    fn loss_on_the_stock_emulator_is_refused_with_the_reason() {
        let (android, root, _adb, emulator) = device_as("stockloss", true);
        let d = android.device("phone").expect("device");
        let err = d.check_network_loss().expect_err("stock");
        assert!(err.contains("stock emulator") && err.contains("no `network loss`") && err.contains("simee boot"), "{err}");
        assert_eq!(d.set_network(Some("full"), None, Some("10")), Err(err));
        assert!(emulator.console.lock().expect("console").is_empty(), "nothing was sent");
        assert_eq!(d.set_network(Some("full"), Some("none"), None).expect("speed"), ["network speed full", "network delay none"]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn airplane_mode_before_android_11_is_the_setting_and_a_broadcast() {
        let (android, root, adb, _emulator) = device("airplane29");
        std::fs::write(root.join("sdk/platform-tools/sdk"), "29\n").expect("sdk");
        let a = android.device("phone").expect("device").adb(&android).expect("adb");
        assert_eq!(a.api_level(), Ok(29));
        a.set_airplane_mode(true).expect("on");
        assert!(a.airplane_mode().expect("read"));
        let ran = a.set_airplane_mode(false).expect("off");
        assert_eq!(ran[0], "settings put global airplane_mode_on 0");
        assert!(!a.airplane_mode().expect("read"));
        let calls = adb.calls();
        assert!(!calls.iter().any(|c| c.contains("cmd connectivity")), "{calls:?}");
        assert!(calls.contains(&"shell am broadcast -a android.intent.action.AIRPLANE_MODE --ez state true".to_owned()), "{calls:?}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn airplane_mode_is_read_and_set_over_adb() {
        let (android, root, adb, _emulator) = device("airplane");
        let a = android.device("phone").expect("device").adb(&android).expect("adb");
        assert!(!a.airplane_mode().expect("read"));
        a.set_airplane_mode(true).expect("on");
        assert!(a.airplane_mode().expect("read"));
        a.set_airplane_mode(false).expect("off");
        assert_eq!(
            adb.calls(),
            [
                "shell settings get global airplane_mode_on",
                "shell getprop ro.build.version.sdk",
                "shell cmd connectivity airplane-mode enable",
                "shell settings get global airplane_mode_on",
                "shell getprop ro.build.version.sdk",
                "shell cmd connectivity airplane-mode disable",
            ]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn sockets_and_traffic_come_from_one_adb_call_each() {
        let (android, root, adb, _emulator) = device("sockets");
        let a = android.device("phone").expect("device").adb(&android).expect("adb");
        let found = a.sockets().expect("sockets");
        assert!(found.as_root, "the fake has su");
        assert!(found.sockets.iter().any(|s| s.pid == Some(4321) && s.process == "com.example.app"), "{:?}", found.sockets);
        let traffic = a.traffic().expect("traffic");
        assert_eq!((traffic[0].uid, traffic[0].process.as_str()), (10086, "com.example.app"));
        assert_eq!(adb.calls().len(), 2, "{:?}", adb.calls());
        std::fs::remove_dir_all(root).ok();
    }
}
