//! The container's own network namespace, and the one way into it: `127.0.0.1:<port>` on the host
//! forwarded to the guest's adbd (port 5555).
//!
//! Redroid's adbd binds 5555 on every address it has and its image runs adb without authentication
//! (`ro.adb.secure=0`, docs/android.md "Container backend"), so a container in the host's network
//! namespace hands a root shell in the guest to anyone who can reach the host. Here the container
//! gets a namespace of its own (`unshare --net`), nothing in the host's namespace listens on a
//! non-loopback address, and the host reaches adb only through a loopback port. Like the rest of
//! the launcher this is data first: [`NetPlan::attach`] says which commands wire the namespace up
//! and what forwards, tests assert it, and [`Session`] runs it.
//!
//! The uplink, in the order [`Uplink::choose`] tries them:
//!
//! - **pasta** (passt): user-mode networking that works unprivileged and as root. Run attached to
//!   the container's namespaces, it copies the host's address and routes into `eth0` (the name
//!   Redroid's init expects) and forwards the loopback port. Everything else is switched off: no
//!   other inbound port (`-t` defaults to every port the guest binds, on every host address), no
//!   guest-to-host-loopback splicing (`-T`, `-U`) and no gateway-to-host mapping (`--no-map-gw`),
//!   so the guest can't reach the host's own loopback services.
//! - **slirp4netns**: the same for hosts without pasta; the port is added through its API socket.
//! - **a veth pair**, root only, for hosts with neither (or no `/dev/net/tun`, which both need):
//!   a /30 per container from the first candidate range the host has no route into, in any
//!   policy-routing table ([`host_routes`]; [`VETH_POOLS`]; not Tailscale's 100.64.0.0/10, which a
//!   tailnet peer could hide behind), held in a [`Reservations`] file so two plans never share
//!   one, with a proxy thread listening on the loopback port.
//!   It has no uplink: the guest reaches the host's side of the pair and nothing further, because
//!   forwarding and masquerading would change the host's firewall. Services the host binds on all
//!   addresses are reachable from the guest at the pair's host address.
//!
//! Root prefers user-mode networking to the veth pair for the same reason: it gives the guest an
//! uplink without touching the host's routing or firewall.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::launch::{Error, Helper, Result, supervised};
use crate::probe::HostView;

/// The guest's interface name: what Redroid's init configures Android's ethernet on.
pub const GUEST_IFNAME: &str = "eth0";
/// adbd's TCP port in the guest.
pub const GUEST_ADB_PORT: u16 = 5555;
/// The address slirp4netns gives the guest.
pub const SLIRP_GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 100);
/// How long an uplink tool has to come up before the launch fails.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);

/// How the container reaches anything outside its namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uplink {
    Pasta,
    Slirp4netns,
    /// A veth pair with a loopback proxy: root only, no uplink.
    Veth,
}

/// What stops the container from having a network of its own, with the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub why: String,
    pub fix: String,
}

impl Uplink {
    pub fn name(self) -> &'static str {
        match self {
            Uplink::Pasta => "pasta",
            Uplink::Slirp4netns => "slirp4netns",
            Uplink::Veth => "a veth pair",
        }
    }

    /// The uplink for the host `view` describes. `ip` is needed whichever it is (it brings the
    /// namespace's loopback up).
    pub fn choose(view: &dyn HostView) -> std::result::Result<Uplink, Missing> {
        if !view.on_path("ip") {
            return Err(Missing {
                why: "ip (iproute2) isn't on PATH, and the container's loopback is brought up with it".into(),
                fix: "sudo apt install iproute2".into(),
            });
        }
        let tun = view.exists("/dev/net/tun");
        if tun {
            for (tool, uplink) in [("pasta", Uplink::Pasta), ("slirp4netns", Uplink::Slirp4netns)] {
                if view.on_path(tool) {
                    return Ok(uplink);
                }
            }
        }
        if view.euid() == 0 {
            return Ok(Uplink::Veth);
        }
        Err(if tun {
            Missing {
                why: "neither pasta nor slirp4netns is on PATH, so an unprivileged user can't give the container a network namespace"
                    .into(),
                fix: "sudo apt install passt (pasta) or slirp4netns, or run as root for a veth pair".into(),
            }
        } else {
            Missing {
                why: "/dev/net/tun is missing, which pasta and slirp4netns need for an unprivileged container network".into(),
                fix: "sudo modprobe tun (CONFIG_TUN), or run as root for a veth pair".into(),
            }
        })
    }
}

/// The two ends of a veth pair's /30.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VethAddrs {
    pub host: Ipv4Addr,
    pub guest: Ipv4Addr,
}

/// Prefix length of the pair's subnet.
pub const VETH_PREFIX: u8 = 30;

/// Where a veth pair's /30 may come from, in order: ranges set aside for private use that no
/// common setup claims (Docker's bridges start at 172.17, LANs sit in 192.168.0/1, Tailscale in
/// 100.64.0.0/10, Kubernetes' service range is 10.96). The first one the host has no route into is
/// used.
pub const VETH_POOLS: [(Ipv4Addr, u8); 5] = [
    (Ipv4Addr::new(10, 233, 0, 0), 16),
    (Ipv4Addr::new(10, 234, 0, 0), 16),
    (Ipv4Addr::new(10, 235, 0, 0), 16),
    (Ipv4Addr::new(172, 29, 0, 0), 16),
    (Ipv4Addr::new(192, 168, 233, 0), 24),
];

/// The host's IPv4 routes as `(destination, mask)`, from `/proc/net/route`'s text. The default
/// route (mask 0) is left out: it overlaps everything and means nothing here.
pub fn parse_routes(proc_net_route: &str) -> Vec<(u32, u32)> {
    // The columns are the kernel's raw network-order words printed as hex, so the bytes are
    // the address as it reads.
    let word = |h: &str| u32::from_str_radix(h, 16).ok().map(|v| u32::from_be_bytes(v.to_ne_bytes()));
    proc_net_route
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (dest, mask) = (word(f.get(1)?)?, word(f.get(7)?)?);
            (mask != 0).then_some((dest, mask))
        })
        .collect()
}

/// `a.b.c.d` or `a.b.c.d/n` as `(network, mask)`; `None` for `default`, `all`, `/0`, IPv6 and
/// anything else that isn't an IPv4 destination.
fn parse_prefix(token: &str) -> Option<(u32, u32)> {
    let (addr, len) = match token.split_once('/') {
        Some((a, n)) => (a, n.parse::<u32>().ok()?),
        None => (token, 32),
    };
    if len == 0 || len > 32 {
        return None;
    }
    let mask = !0u32 << (32 - len);
    Some((u32::from(addr.parse::<Ipv4Addr>().ok()?) & mask, mask))
}

/// The routes in `ip -4 route show table all` (or `table <id>`), every table at once: the main
/// table, `local` and the ones `ip rule` selects (Tailscale's 52, a VPN's). A line starts with an
/// optional route type, then the destination. `default` routes are left out like in
/// [`parse_routes`], and so are `throw` routes, which only send the lookup on to the next rule.
pub fn parse_ip_routes(text: &str) -> Vec<(u32, u32)> {
    const TYPES: [&str; 9] = ["unicast", "local", "broadcast", "multicast", "anycast", "blackhole", "unreachable", "prohibit", "nat"];
    text.lines()
        .filter_map(|l| {
            let mut t = l.split_whitespace().peekable();
            if t.next_if_eq(&"throw").is_some() {
                return None;
            }
            t.next_if(|w| TYPES.contains(w));
            parse_prefix(t.next()?)
        })
        .collect()
}

/// What `ip -4 rule` says about routing: the destination prefixes rules select on (`to P`: a rule
/// that sends a range to its own table or verdict claims it as much as a route does) and the
/// tables they look up.
pub fn parse_ip_rules(text: &str) -> (Vec<(u32, u32)>, Vec<String>) {
    let (mut prefixes, mut tables) = (Vec::new(), Vec::new());
    for l in text.lines() {
        let t: Vec<&str> = l.split_whitespace().collect();
        for w in t.windows(2) {
            match w[0] {
                "to" => prefixes.extend(parse_prefix(w[1])),
                "lookup" | "table" if !tables.iter().any(|x| x == w[1]) => tables.push(w[1].to_string()),
                _ => {}
            }
        }
    }
    (prefixes, tables)
}

/// Every IPv4 destination the host routes somewhere: the main table from `/proc/net/route`,
/// plus every table `ip rule` can select, so a policy route (Tailscale's table 52, a VPN's) counts
/// as much as one in main. Without `ip` it is the main table alone.
pub fn host_routes(view: &dyn HostView) -> Vec<(u32, u32)> {
    let mut routes = parse_routes(&view.read("/proc/net/route").unwrap_or_default());
    let (prefixes, tables) = parse_ip_rules(&view.ip(&["-4", "rule", "show"]).unwrap_or_default());
    routes.extend(prefixes);
    match view.ip(&["-4", "route", "show", "table", "all"]) {
        Some(all) => routes.extend(parse_ip_routes(&all)),
        // Older or restricted iproute2: ask for the tables the rules name, one by one.
        None => {
            for id in tables {
                routes.extend(parse_ip_routes(&view.ip(&["-4", "route", "show", "table", &id]).unwrap_or_default()));
            }
        }
    }
    routes
}

fn is_main(table: &str) -> bool {
    matches!(table, "main" | "254")
}

/// The tables with a default route that `ip rule` consults before the main table, for any
/// traffic (a rule with no selector but `from all`): what an exit node or a full-tunnel VPN does
/// (Tailscale's table 52). A veth pair's traffic would leave through such a table, whatever /30
/// it has. `route_text` is `ip -4 route show table all` (lines say `table <id>`); `only` names
/// the table when the text is one table's listing, which doesn't say.
pub fn early_default_tables(rule_text: &str, route_text: &str, only: Option<&str>) -> Vec<String> {
    let mut rules: Vec<(u32, &str)> = Vec::new();
    for l in rule_text.lines() {
        let Some((prio, rest)) = l.split_once(':') else { continue };
        let Ok(prio) = prio.trim().parse::<u32>() else { continue };
        let t: Vec<&str> = rest.split_whitespace().collect();
        if t.len() == 4 && t[..3] == ["from", "all", "lookup"] {
            rules.push((prio, t[3]));
        }
    }
    let main_prio = rules.iter().filter(|(_, t)| is_main(t)).map(|&(p, _)| p).min().unwrap_or(32766);
    let mut found: Vec<String> = Vec::new();
    for l in route_text.lines() {
        let mut t = l.split_whitespace().peekable();
        if t.peek() == Some(&"throw") {
            continue;
        }
        t.next_if(|w| ["unicast", "blackhole", "unreachable", "prohibit"].contains(w));
        if t.next() != Some("default") {
            continue;
        }
        let table = l.split_whitespace().skip_while(|w| *w != "table").nth(1).or(only);
        let Some(table) = table else { continue };
        let early = rules.iter().any(|&(prio, t)| t == table && prio < main_prio);
        if early && !is_main(table) && !found.iter().any(|f| f == table) {
            found.push(table.to_string());
        }
    }
    found
}

/// The tables `ip rule` consults before main that hold a default route ([`early_default_tables`]),
/// read from `view`.
fn host_early_defaults(view: &dyn HostView) -> Vec<String> {
    let rules = view.ip(&["-4", "rule", "show"]).unwrap_or_default();
    match view.ip(&["-4", "route", "show", "table", "all"]) {
        Some(all) => early_default_tables(&rules, &all, None),
        None => parse_ip_rules(&rules)
            .1
            .iter()
            .flat_map(|id| early_default_tables(&rules, &view.ip(&["-4", "route", "show", "table", id]).unwrap_or_default(), Some(id)))
            .collect(),
    }
}

/// Whether `/30` block `block` overlaps a route: they agree on the bits both fix.
fn routed(block: u32, routes: &[(u32, u32)]) -> bool {
    let block_mask = !0u32 << (32 - u32::from(VETH_PREFIX));
    routes.iter().any(|&(dest, mask)| {
        let common = mask & block_mask;
        block & common == dest & common
    })
}

/// The pair's addresses for a container whose adb port is `adb_port`: the /30 of the first of
/// [`VETH_POOLS`] that `free` accepts and no route in `routes` reaches into, starting at block
/// number `adb_port` of the pool and going on to the next block when that one is taken. The error
/// lists what was tried.
fn pick_block(adb_port: u16, routes: &[(u32, u32)], free: &mut dyn FnMut(u32) -> bool) -> Result<VethAddrs> {
    for (base, prefix) in VETH_POOLS {
        let blocks = 1u32 << (VETH_PREFIX - prefix);
        for i in 0..blocks {
            let block = u32::from(base) + 4 * ((u32::from(adb_port) + i) % blocks);
            if !routed(block, routes) && free(block) {
                return Ok(VethAddrs { host: Ipv4Addr::from(block + 1), guest: Ipv4Addr::from(block + 2) });
            }
        }
    }
    let tried: Vec<String> = VETH_POOLS.iter().map(|(a, p)| format!("{a}/{p}")).collect();
    Err(Error::Net(format!(
        "the host has a route into, or another container holds, every /30 in the ranges a veth pair may use ({}); remove one of those routes, stop a container, or install pasta or slirp4netns (with /dev/net/tun) to avoid the pair",
        tried.join(", ")
    )))
}

/// The pair's addresses for `adb_port` against `routes` alone: [`Reservations::claim`] without
/// the claim, for planning on paper.
pub fn veth_addrs(adb_port: u16, routes: &[(u32, u32)]) -> Result<VethAddrs> {
    pick_block(adb_port, routes, &mut |_| true)
}

/// The /30s containers on this host hold, one file per block in one directory (`/run/simee/veth`):
/// the file is named for the block's network address and holds the pid of the process that
/// planned it. A claim is made under an exclusive `flock` on the directory's `.lock`, so planners
/// (threads, processes, different users' launchers) take turns and never see each other's
/// half-made claims; a file whose pid is gone (a crashed launcher) is taken over.
#[derive(Debug, Clone)]
pub struct Reservations {
    dir: PathBuf,
}

/// A held /30: the file is removed when the last handle drops (the container stopped).
#[derive(Debug, PartialEq, Eq)]
pub struct Reservation {
    path: PathBuf,
    pid: u32,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Only our own claim: a file someone else took over (our pid looked gone) stays.
        if std::fs::read_to_string(&self.path).ok().and_then(|t| t.trim().parse::<u32>().ok()) == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether process `pid` exists (it may belong to another user: `EPERM` still means it's there).
fn pid_alive(pid: u32) -> bool {
    use rustix::process::{Pid, test_kill_process};
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else { return false };
    !matches!(test_kill_process(pid), Err(rustix::io::Errno::SRCH))
}

impl Reservations {
    pub fn new(dir: impl Into<PathBuf>) -> Reservations {
        Reservations { dir: dir.into() }
    }

    fn io(&self, what: &str, e: std::io::Error) -> Error {
        Error::Net(format!("{what} in {}: {e} (veth pairs need a directory every launcher on the host can write)", self.dir.display()))
    }

    /// Picks a /30 as [`veth_addrs`] does, skipping the ones held, and holds it.
    pub fn claim(&self, adb_port: u16, routes: &[(u32, u32)]) -> Result<(VethAddrs, Reservation)> {
        std::fs::create_dir_all(&self.dir).map_err(|e| self.io("creating the reservation directory", e))?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".lock"))
            .map_err(|e| self.io("opening the lock", e))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).map_err(|e| self.io("locking", e.into()))?;
        let pid = std::process::id();
        let (mut held, mut failed) = (None, None);
        let picked = pick_block(adb_port, routes, &mut |block| {
            let path = self.dir.join(Ipv4Addr::from(block).to_string());
            let owner = std::fs::read_to_string(&path).ok().and_then(|t| t.trim().parse::<u32>().ok());
            // A file nobody can read a pid from is as dead as one whose process is gone.
            if path.exists() && owner.is_some_and(pid_alive) {
                return false;
            }
            if let Err(e) = std::fs::write(&path, format!("{pid}\n")) {
                failed = Some(self.io("writing a reservation", e));
                return false;
            }
            held = Some(Reservation { path, pid });
            true
        });
        // Every block failing to write is the directory's fault, not the pool's.
        let addrs = match (picked, failed) {
            (Ok(a), _) => a,
            (Err(_), Some(e)) => return Err(e),
            (Err(e), None) => return Err(e),
        };
        // The lock drops with `lock`, after the file is written.
        Ok((addrs, held.expect("a picked block is held")))
    }
}

/// What a container's network is, as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetPlan {
    pub uplink: Uplink,
    /// The host's loopback port that reaches the guest's adbd.
    pub adb_port: u16,
    /// The pair's addresses: set exactly when the uplink is [`Uplink::Veth`].
    pub veth: Option<VethAddrs>,
    /// The hold on the pair's /30, kept for as long as the plan or the [`Session`] made from it
    /// lives, so no other plan on the host can be given the same subnet.
    pub reservation: Option<Arc<Reservation>>,
}

/// A long-running process that serves the namespace for as long as the container lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
    pub argv: Vec<String>,
    /// Where it says it is ready.
    pub ready: Ready,
    /// Where its output goes.
    pub log: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    /// A file with its pid in it.
    PidFile(PathBuf),
    /// A unix socket that accepts connections.
    Socket(PathBuf),
}

/// What the parent does, once the container's namespaces exist and before `/init` is released.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attach {
    /// Commands run to completion, in order.
    pub setup: Vec<Vec<String>>,
    pub daemon: Option<Daemon>,
    /// A request for the daemon's API socket (the socket, the JSON line).
    pub api_call: Option<(PathBuf, String)>,
    /// A loopback proxy `(listen, connect to)` for an uplink with no forwarding of its own.
    pub proxy: Option<(SocketAddr, SocketAddr)>,
}

impl NetPlan {
    /// `routes` are the host's ([`host_routes`]), which a veth pair's addresses must not overlap;
    /// `reservations` is where the pair's /30 is held against other plans.
    pub fn new(uplink: Uplink, adb_port: u16, routes: &[(u32, u32)], reservations: &Reservations) -> Result<NetPlan> {
        if adb_port == 0 {
            return Err(Error::Invalid("the adb port must be a loopback port, not 0".into()));
        }
        let (veth, reservation) = match uplink {
            Uplink::Veth => {
                let (addrs, held) = reservations.claim(adb_port, routes)?;
                (Some(addrs), Some(Arc::new(held)))
            }
            _ => (None, None),
        };
        Ok(NetPlan { uplink, adb_port, veth, reservation })
    }

    /// The plan for the host `view` describes; the error says what to install.
    pub fn for_host(view: &dyn HostView, adb_port: u16) -> Result<NetPlan> {
        let uplink = Uplink::choose(view).map_err(|m| Error::Net(format!("{} (fix: {})", m.why, m.fix)))?;
        // Routes are only read for a pair: the other uplinks don't take an address from the host.
        let mut routes = Vec::new();
        if uplink == Uplink::Veth {
            if let Some(table) = host_early_defaults(view).first() {
                return Err(Error::Net(format!(
                    "the host routes all traffic through table {table} (a default route there, which `ip rule` consults before the main table: an exit node or a full-tunnel VPN), so a veth pair's traffic would leave through it; turn that off, or install pasta or slirp4netns (with /dev/net/tun) to avoid the pair"
                )));
            }
            routes = host_routes(view);
        }
        NetPlan::new(uplink, adb_port, &routes, &Reservations::new(view.reservations_dir()))
    }

    /// The commands the container's setup script runs in its own namespace, before `/init`.
    pub fn guest_commands(&self) -> Vec<Vec<String>> {
        let ip = |args: &[&str]| ["ip"].iter().chain(args).map(|s| s.to_string()).collect::<Vec<_>>();
        let mut cmds = vec![ip(&["link", "set", "lo", "up"])];
        if let Some(a) = self.veth {
            cmds.push(ip(&["addr", "add", &format!("{}/{VETH_PREFIX}", a.guest), "dev", GUEST_IFNAME]));
            cmds.push(ip(&["link", "set", GUEST_IFNAME, "up"]));
        }
        cmds
    }

    /// What the host does for the container whose namespaces process `pid` holds. `dir` is a
    /// directory the launching user can write (not the container's own, which belongs to
    /// container root).
    pub fn attach(&self, pid: u32, dir: &Path) -> Attach {
        let loopback = Ipv4Addr::LOCALHOST;
        let log = dir.join("net.log");
        match self.uplink {
            Uplink::Pasta => {
                let pid_file = dir.join("net-pasta.pid");
                let argv = [
                    "pasta",
                    "--foreground",
                    "--quiet",
                    "--pid",
                    &pid_file.display().to_string(),
                    "--config-net",
                    "-I",
                    GUEST_IFNAME,
                    "-4",
                    "--no-map-gw",
                    // Nothing in either direction but the one port: no auto-forwarded guest ports,
                    // no guest access to the host's loopback, no UDP.
                    "-T",
                    "none",
                    "-U",
                    "none",
                    "-u",
                    "none",
                    "-t",
                    &format!("{loopback}/{}:{GUEST_ADB_PORT}", self.adb_port),
                    &pid.to_string(),
                ];
                Attach {
                    daemon: Some(Daemon { argv: argv.map(String::from).to_vec(), ready: Ready::PidFile(pid_file), log }),
                    ..Attach::default()
                }
            }
            Uplink::Slirp4netns => {
                let socket = dir.join("net-slirp.sock");
                let argv = [
                    "slirp4netns",
                    "--configure",
                    "--mtu=65520",
                    "--disable-host-loopback",
                    "--api-socket",
                    &socket.display().to_string(),
                    &pid.to_string(),
                    GUEST_IFNAME,
                ];
                let call = serde_json::json!({
                    "execute": "add_hostfwd",
                    "arguments": {
                        "proto": "tcp",
                        "host_addr": loopback.to_string(),
                        "host_port": self.adb_port,
                        "guest_addr": SLIRP_GUEST.to_string(),
                        "guest_port": GUEST_ADB_PORT,
                    },
                })
                .to_string();
                Attach {
                    daemon: Some(Daemon { argv: argv.map(String::from).to_vec(), ready: Ready::Socket(socket.clone()), log }),
                    api_call: Some((socket, call)),
                    ..Attach::default()
                }
            }
            Uplink::Veth => {
                let a = self.veth.expect("a veth plan has its addresses");
                let host_if = format!("sveth{}", self.adb_port);
                let ip = |args: &[&str]| ["ip"].iter().chain(args).map(|s| s.to_string()).collect::<Vec<_>>();
                Attach {
                    // The peer is created inside the container's namespace, already named `eth0`.
                    setup: vec![
                        ip(&["link", "add", &host_if, "type", "veth", "peer", "name", GUEST_IFNAME, "netns", &pid.to_string()]),
                        ip(&["addr", "add", &format!("{}/{VETH_PREFIX}", a.host), "dev", &host_if]),
                        ip(&["link", "set", &host_if, "up"]),
                    ],
                    proxy: Some((SocketAddr::from((loopback, self.adb_port)), SocketAddr::from((a.guest, GUEST_ADB_PORT)))),
                    ..Attach::default()
                }
            }
        }
    }
}

/// A TCP proxy from a loopback port to one address, on a thread that stops when this is dropped
/// (connections already open end when either side closes them).
pub struct Forwarder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    local: SocketAddr,
}

impl Forwarder {
    /// Listens on `listen`, which must be a loopback address, and forwards each connection to `to`.
    pub fn start(listen: SocketAddr, to: SocketAddr) -> std::io::Result<Forwarder> {
        if !listen.ip().is_loopback() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{listen} isn't a loopback address")));
        }
        let listener = TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new().name("adb-forward".into()).spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((client, _)) => proxy(client, to),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        })?;
        Ok(Forwarder { stop, thread: Some(thread), local })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Connects `client` to `to` and copies both ways on two threads.
fn proxy(client: TcpStream, to: SocketAddr) {
    let Ok(upstream) = TcpStream::connect_timeout(&to, Duration::from_secs(2)) else { return };
    // `accept` on a nonblocking listener can hand back a nonblocking stream.
    if client.set_nonblocking(false).is_err() {
        return;
    }
    let (Ok(mut c_in), Ok(mut u_out)) = (client.try_clone(), upstream.try_clone()) else { return };
    let (mut u_in, mut c_out) = (upstream, client);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_in, &mut u_out);
        let _ = u_out.shutdown(Shutdown::Write);
    });
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut u_in, &mut c_out);
        let _ = c_out.shutdown(Shutdown::Write);
    });
}

/// The processes and threads that give a running container its network. Dropping it stops them.
#[derive(Default)]
pub struct Session {
    daemons: Vec<Child>,
    forwarder: Option<Forwarder>,
    /// The pair's /30 stays held until the container's network is gone.
    _reservation: Option<Arc<Reservation>>,
}

impl Session {
    /// Wires the namespaces of process `pid` up as `plan` says. With a `helper` the daemon dies
    /// with this process (see [`supervised`]).
    pub fn start(plan: &NetPlan, pid: u32, dir: &Path, helper: Option<&Helper>) -> Result<Session> {
        let attach = plan.attach(pid, dir);
        // Any error returns early and drops `session`, which stops what already started.
        let mut session = Session::default();
        session._reservation = plan.reservation.clone();
        for argv in &attach.setup {
            let out = Command::new(&argv[0]).args(&argv[1..]).output().map_err(|e| Error::Net(format!("running {}: {e}", argv[0])))?;
            if !out.status.success() {
                return Err(Error::Net(format!("`{}` failed: {}", argv.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
            }
        }
        if let Some(daemon) = &attach.daemon {
            let argv = match helper {
                Some(h) => supervised(h, std::process::id(), &daemon.argv),
                None => daemon.argv.clone(),
            };
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&daemon.log)
                .map_err(|e| Error::Net(format!("opening {}: {e}", daemon.log.display())))?;
            // A stale file from an earlier run would read as "ready".
            match &daemon.ready {
                Ready::PidFile(p) | Ready::Socket(p) => {
                    let _ = std::fs::remove_file(p);
                }
            }
            let child = Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(log.try_clone().map_err(|e| Error::Net(format!("duplicating the log handle: {e}")))?)
                .stderr(log)
                .spawn()
                .map_err(|e| Error::Net(format!("starting {}: {e} (is it installed?)", daemon.argv[0])))?;
            session.daemons.push(child);
            let child = session.daemons.last_mut().expect("just pushed");
            wait_ready(child, &daemon.ready, &daemon.log, READY_DEADLINE)?;
        }
        if let Some((socket, call)) = &attach.api_call {
            api_call(socket, call)?;
        }
        if let Some((listen, to)) = attach.proxy {
            session.forwarder = Some(Forwarder::start(listen, to).map_err(|e| Error::Net(format!("listening on {listen}: {e}")))?);
        }
        Ok(session)
    }

    /// Whether every daemon is still running.
    pub fn healthy(&mut self) -> bool {
        self.daemons.iter_mut().all(|c| matches!(c.try_wait(), Ok(None)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for c in &mut self.daemons {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The last few hundred bytes of a daemon's log, for an error message.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let tail: String = text.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    tail.trim().to_string()
}

/// Waits until `child` says it is ready, it exits, or `deadline` passes.
pub fn wait_ready(child: &mut Child, ready: &Ready, log: &Path, deadline: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|e| Error::Net(format!("waiting for the network helper: {e}")))? {
            return Err(Error::Net(format!("the network helper exited ({status}): {}", log_tail(log))));
        }
        let up = match ready {
            Ready::PidFile(p) => std::fs::read_to_string(p).is_ok_and(|s| !s.trim().is_empty()),
            Ready::Socket(p) => UnixStream::connect(p).is_ok(),
        };
        if up {
            return Ok(());
        }
        if start.elapsed() >= deadline {
            return Err(Error::Net(format!("the network helper wasn't ready within {deadline:?}: {}", log_tail(log))));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Sends one JSON line to an API socket and requires a `return` in the answer.
pub fn api_call(socket: &Path, call: &str) -> Result<()> {
    let net = |what: &str, e: std::io::Error| Error::Net(format!("{what} {}: {e}", socket.display()));
    let mut s = UnixStream::connect(socket).map_err(|e| net("connecting to", e))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| net("configuring", e))?;
    s.write_all(call.as_bytes()).and_then(|()| s.write_all(b"\n")).map_err(|e| net("writing to", e))?;
    s.shutdown(Shutdown::Write).map_err(|e| net("writing to", e))?;
    let mut answer = String::new();
    s.read_to_string(&mut answer).map_err(|e| net("reading from", e))?;
    let ok = serde_json::from_str::<serde_json::Value>(answer.trim()).is_ok_and(|v| v.get("return").is_some());
    if ok { Ok(()) } else { Err(Error::Net(format!("the port forward was refused: {}", answer.trim()))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeView;

    fn view(tools: &[&str], tun: bool, euid: u32) -> FakeView {
        let mut v = FakeView::container_host();
        v.euid = euid;
        v.tools.clear();
        for t in tools {
            v.tools.insert((*t).to_string());
        }
        if tun {
            v.paths.insert("/dev/net/tun".into());
        } else {
            v.paths.remove("/dev/net/tun");
        }
        v
    }

    #[test]
    fn pasta_is_preferred_then_slirp_then_a_veth_pair_for_root() {
        assert_eq!(Uplink::choose(&view(&["ip", "pasta", "slirp4netns"], true, 1000)), Ok(Uplink::Pasta));
        assert_eq!(Uplink::choose(&view(&["ip", "slirp4netns"], true, 1000)), Ok(Uplink::Slirp4netns));
        // Root takes user-mode networking over the veth pair: it has an uplink and leaves the
        // host's firewall alone.
        assert_eq!(Uplink::choose(&view(&["ip", "pasta"], true, 0)), Ok(Uplink::Pasta));
        assert_eq!(Uplink::choose(&view(&["ip"], true, 0)), Ok(Uplink::Veth));
        // Without /dev/net/tun neither tool works, whoever runs them.
        assert_eq!(Uplink::choose(&view(&["ip", "pasta", "slirp4netns"], false, 0)), Ok(Uplink::Veth));
    }

    #[test]
    fn an_unprivileged_user_without_a_tool_or_tun_is_refused_with_the_fix() {
        let no_tool = Uplink::choose(&view(&["ip"], true, 1000)).expect_err("no tool");
        assert!(no_tool.why.contains("neither pasta nor slirp4netns"), "{no_tool:?}");
        assert!(no_tool.fix.contains("apt install passt") && no_tool.fix.contains("slirp4netns"), "{no_tool:?}");
        let no_tun = Uplink::choose(&view(&["ip", "pasta"], false, 1000)).expect_err("no tun");
        assert!(no_tun.why.contains("/dev/net/tun") && no_tun.fix.contains("modprobe tun"), "{no_tun:?}");
        let no_ip = Uplink::choose(&view(&["pasta"], true, 0)).expect_err("no ip");
        assert!(no_ip.fix.contains("iproute2"), "{no_ip:?}");
        let e = NetPlan::for_host(&view(&["ip"], true, 1000), 5557).expect_err("refused").to_string();
        assert!(e.contains("fix: sudo apt install passt"), "{e}");
    }

    #[test]
    fn the_adb_port_must_be_a_real_port() {
        assert!(NetPlan::new(Uplink::Pasta, 0, &[], &scratch_reservations()).is_err());
        assert_eq!(NetPlan::new(Uplink::Pasta, 5555, &[], &scratch_reservations()).expect("plan").adb_port, 5555);
    }

    #[test]
    fn pasta_forwards_the_loopback_port_and_nothing_else() {
        let dir = Path::new("/run/avd");
        let a = NetPlan::new(Uplink::Pasta, 5557, &[], &scratch_reservations()).expect("plan").attach(4242, dir);
        let d = a.daemon.expect("pasta runs as a daemon");
        let argv = d.argv.join(" ");
        assert_eq!(d.argv[0], "pasta");
        // Attached to the container's own namespaces, not a new one.
        assert_eq!(d.argv.last().map(String::as_str), Some("4242"));
        assert!(argv.contains("-t 127.0.0.1/5557:5555"), "{argv}");
        // No other port, no spliced loopback, no gateway-to-host mapping: all explicit, because
        // pasta's defaults forward every port the guest binds on every host address.
        for off in ["-T none", "-U none", "-u none", "--no-map-gw"] {
            assert!(argv.contains(off), "{off} missing: {argv}");
        }
        assert!(!argv.contains("0.0.0.0") && !argv.contains("-t auto"), "{argv}");
        assert_eq!(d.argv.iter().filter(|a| *a == "-t").count(), 1, "one inbound rule: {argv}");
        assert!(argv.contains("-I eth0") && argv.contains("--config-net"), "{argv}");
        assert_eq!(d.ready, Ready::PidFile(dir.join("net-pasta.pid")));
        assert!(argv.contains(&format!("--pid {}", dir.join("net-pasta.pid").display())));
        assert!(a.setup.is_empty() && a.proxy.is_none() && a.api_call.is_none());
    }

    #[test]
    fn slirp4netns_adds_one_loopback_forward_through_its_api() {
        let dir = Path::new("/run/avd");
        let a = NetPlan::new(Uplink::Slirp4netns, 5557, &[], &scratch_reservations()).expect("plan").attach(4242, dir);
        let d = a.daemon.expect("slirp4netns runs as a daemon");
        let argv = d.argv.join(" ");
        assert!(argv.contains("--disable-host-loopback") && argv.contains("--configure"), "{argv}");
        assert!(argv.ends_with("4242 eth0"), "{argv}");
        let (socket, call) = a.api_call.expect("the forward goes through the API");
        assert_eq!(d.ready, Ready::Socket(socket.clone()));
        assert!(argv.contains(&format!("--api-socket {}", socket.display())), "{argv}");
        let v: serde_json::Value = serde_json::from_str(&call).expect("JSON");
        assert_eq!(v["execute"], "add_hostfwd");
        assert_eq!(v["arguments"]["host_addr"], "127.0.0.1", "the host side is loopback only");
        assert_eq!(v["arguments"]["host_port"], 5557);
        assert_eq!(v["arguments"]["guest_port"], 5555);
        assert_eq!(v["arguments"]["guest_addr"], SLIRP_GUEST.to_string());
    }

    #[test]
    fn the_veth_pair_moves_its_peer_into_the_container_and_proxies_on_loopback() {
        let plan = NetPlan::new(Uplink::Veth, 5557, &[], &scratch_reservations()).expect("plan");
        let a = plan.attach(4242, Path::new("/run/avd"));
        let a_s: Vec<String> = a.setup.iter().map(|c| c.join(" ")).collect();
        assert_eq!(a_s[0], "ip link add sveth5557 type veth peer name eth0 netns 4242");
        assert!(a_s[1].starts_with("ip addr add 10.233.") && a_s[1].ends_with("/30 dev sveth5557"), "{a_s:?}");
        assert_eq!(a_s[2], "ip link set sveth5557 up");
        let addrs = plan.veth.expect("addresses");
        let (listen, to) = a.proxy.expect("a proxy");
        assert_eq!(listen, "127.0.0.1:5557".parse().expect("addr"), "the host side listens on loopback only");
        assert_eq!(to, SocketAddr::from((addrs.guest, 5555)));
        assert!(a.daemon.is_none());
        // The guest's own end is configured by its setup script, inside its namespace.
        let g: Vec<String> = plan.guest_commands().iter().map(|c| c.join(" ")).collect();
        assert_eq!(g[0], "ip link set lo up");
        assert_eq!(g[1], format!("ip addr add {}/30 dev eth0", addrs.guest));
        assert_eq!(g[2], "ip link set eth0 up");
    }

    #[test]
    fn user_mode_uplinks_leave_the_guests_interface_to_the_tool() {
        for uplink in [Uplink::Pasta, Uplink::Slirp4netns] {
            let g = NetPlan::new(uplink, 5557, &[], &scratch_reservations()).expect("plan").guest_commands();
            assert_eq!(g, vec![["ip", "link", "set", "lo", "up"].map(String::from).to_vec()], "{uplink:?}");
        }
    }

    #[test]
    fn every_uplink_publishes_only_on_loopback() {
        for uplink in [Uplink::Pasta, Uplink::Slirp4netns, Uplink::Veth] {
            let a = NetPlan::new(uplink, 40000, &[], &scratch_reservations()).expect("plan").attach(1, Path::new("/d"));
            let text = format!("{a:?}");
            assert!(!text.contains("0.0.0.0") && !text.contains("::"), "{uplink:?}: {text}");
            assert!(text.contains("127.0.0.1"), "{uplink:?} names the loopback address: {text}");
        }
    }

    /// What `/proc/net/route` prints (little-endian hex words), for a host on Tailscale and Docker.
    const ROUTES: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
eth0\t0000A8C0\t00000000\t0001\t0\t0\t100\t0000FFFF\t0\t0\t0\n\
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
tailscale0\t00004064\t00000000\t0001\t0\t0\t0\t0000C0FF\t0\t0\t0\n";

    #[test]
    fn routes_are_read_from_proc_net_route_without_the_default() {
        let r = parse_routes(ROUTES);
        assert_eq!(r.len(), 3, "the default route is left out: {r:?}");
        assert!(r.contains(&(u32::from(Ipv4Addr::new(192, 168, 0, 0)), 0xFFFF_0000)), "{r:?}");
        assert!(r.contains(&(u32::from(Ipv4Addr::new(172, 17, 0, 0)), 0xFFFF_0000)), "{r:?}");
        // Tailscale's 100.64.0.0/10 (what the pair used to take from).
        assert!(r.contains(&(u32::from(Ipv4Addr::new(100, 64, 0, 0)), 0xFFC0_0000)), "{r:?}");
        assert!(parse_routes("").is_empty() && parse_routes("Iface\tDestination\n").is_empty());
        assert!(parse_routes("x\neth0\tzz\n").is_empty(), "a garbled line is skipped");
    }

    #[test]
    fn veth_subnets_are_distinct_slash_30s_in_a_private_range_the_host_doesnt_route() {
        let routes = parse_routes(ROUTES);
        let (a, b) = (veth_addrs(5557, &routes).expect("a"), veth_addrs(5559, &routes).expect("b"));
        assert_ne!(a.host, b.host);
        for p in [1u16, 5555, 5557, 40000, 65535] {
            let v = veth_addrs(p, &routes).expect("addresses");
            let (h, g) = (u32::from(v.host), u32::from(v.guest));
            assert_eq!(g, h + 1);
            assert_eq!(h & 3, 1, "host is .1 of its /30");
            assert_eq!(v.host.octets()[..2], [10, 233], "the first pool the host doesn't route: {v:?}");
        }
        // 5557 % 16384 = 5557: block 5557 of 10.233.0.0/16.
        assert_eq!(veth_addrs(5557, &routes).expect("addresses").host, Ipv4Addr::new(10, 233, 86, 213));
        // Never Tailscale's range, whatever the routes.
        for p in [0u16, 1, 65535] {
            let h = veth_addrs(p, &[]).expect("addresses").host;
            assert!(h.octets()[0] != 100, "{h}");
        }
    }

    #[test]
    fn a_pool_the_host_routes_into_is_skipped_a_routed_block_is_stepped_over_and_all_taken_is_refused_with_the_list() {
        let net = |a: [u8; 4], mask: u32| (u32::from(Ipv4Addr::from(a)), mask);
        // A route over all of 10.233.0.0/16, and a /32 inside 10.234's block for this port.
        let taken_233 = [net([10, 233, 0, 0], 0xFFFF_0000)];
        assert_eq!(veth_addrs(5557, &taken_233).expect("next pool").host.octets()[..2], [10, 234]);
        let in_234 = [net([10, 233, 0, 0], 0xFFFF_0000), net([10, 234, 86, 213], 0xFFFF_FFFF)];
        // The next block of the same pool, not the whole pool, goes: 5558 in 10.234.
        assert_eq!(veth_addrs(5557, &in_234).expect("next block").host, Ipv4Addr::new(10, 234, 86, 217));
        // A route elsewhere in the same pool, off this block, leaves it usable.
        let elsewhere = [net([10, 233, 0, 0], 0xFFFF_FF00)];
        assert_eq!(veth_addrs(5557, &elsewhere).expect("same pool").host.octets()[..2], [10, 233]);
        // A broad route (10.0.0.0/8) takes every 10.x pool; 172.29 is next.
        let ten = [net([10, 0, 0, 0], 0xFF00_0000)];
        assert_eq!(veth_addrs(5557, &ten).expect("172.29").host.octets()[..2], [172, 29]);
        // Everything routed: refused, naming each range.
        let all = [net([10, 0, 0, 0], 0xFF00_0000), net([172, 16, 0, 0], 0xFFF0_0000), net([192, 168, 0, 0], 0xFFFF_0000)];
        let e = veth_addrs(5557, &all).expect_err("all taken").to_string();
        for (a, p) in VETH_POOLS {
            assert!(e.contains(&format!("{a}/{p}")), "{e}");
        }
        assert!(e.contains("pasta or slirp4netns"), "{e}");
    }

    #[test]
    fn the_plan_reads_the_hosts_routes() {
        let on_tailscale = view(&["ip"], true, 0).file("/proc/net/route", ROUTES);
        let plan = NetPlan::for_host(&on_tailscale, 5557).expect("plan");
        assert_eq!(plan.uplink, Uplink::Veth);
        assert_eq!(plan.veth.expect("addresses").host, Ipv4Addr::new(10, 233, 86, 213));
        // A host that routes every candidate can't have the pair: refused at plan time.
        let all = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
eth0\t0000000A\t00000000\t0001\t0\t0\t0\t000000FF\n\
eth0\t000010AC\t00000000\t0001\t0\t0\t0\t0000F0FF\n\
eth0\t0000A8C0\t00000000\t0001\t0\t0\t0\t0000FFFF\n";
        let e = NetPlan::for_host(&view(&["ip"], true, 0).file("/proc/net/route", all), 5557).expect_err("all routed").to_string();
        assert!(e.contains("every /30 in the ranges"), "{e}");
        // User-mode uplinks don't use the pair's addresses, whatever the routes.
        let pasta = view(&["ip", "pasta"], true, 1000).file("/proc/net/route", all);
        assert_eq!(NetPlan::for_host(&pasta, 5557).expect("plan").veth, None);
    }

    /// `ip -4 route show table all` on a host with Tailscale (table 52) and Docker: the main table,
    /// the `local` table's entries, and a throw route that must not count.
    const IP_ROUTES: &str = "default via 192.168.0.1 dev eth0 proto dhcp metric 100\n\
172.17.0.0/16 dev docker0 proto kernel scope link src 172.17.0.1 linkdown\n\
192.168.0.0/16 dev eth0 proto kernel scope link src 192.168.0.7 metric 100\n\
100.100.100.100 dev tailscale0 table 52\n\
10.233.86.0/24 dev tun7 table 52\n\
throw 10.0.0.0/8 table 52\n\
broadcast 192.168.255.255 dev eth0 table local proto kernel scope link src 192.168.0.7\n\
local 192.168.0.7 dev eth0 table local proto kernel scope host src 192.168.0.7\n\
local 127.0.0.0/8 dev lo table local proto kernel scope host src 127.0.0.1\n\
fe80::/64 dev eth0 proto kernel metric 256 pref medium\n";

    /// `ip -4 rule` with Tailscale's rules and a rule that sends a range to a table by destination.
    const IP_RULES: &str = "0:\tfrom all lookup local\n\
5210:\tfrom all fwmark 0x80000/0xff0000 lookup main\n\
5270:\tfrom all lookup 52\n\
5300:\tfrom all to 10.234.0.0/16 lookup 77\n\
32766:\tfrom all lookup main\n\
32767:\tfrom all lookup default\n";

    #[test]
    fn routes_in_every_policy_table_are_read_from_ip_output() {
        let r = parse_ip_routes(IP_ROUTES);
        let net = |a: [u8; 4], mask: u32| (u32::from(Ipv4Addr::from(a)), mask);
        assert!(r.contains(&net([10, 233, 86, 0], 0xFFFF_FF00)), "a route in table 52: {r:?}");
        assert!(r.contains(&net([100, 100, 100, 100], 0xFFFF_FFFF)), "a bare address is a /32: {r:?}");
        assert!(r.contains(&net([192, 168, 0, 7], 0xFFFF_FFFF)), "the local table counts: {r:?}");
        assert!(r.contains(&net([172, 17, 0, 0], 0xFFFF_0000)), "{r:?}");
        assert!(!r.iter().any(|&(_, m)| m == 0), "default routes are left out: {r:?}");
        assert!(!r.contains(&net([10, 0, 0, 0], 0xFF00_0000)), "a throw route reaches nothing: {r:?}");
        assert_eq!(r.len(), 7, "IPv6 lines are skipped: {r:?}");
        assert!(parse_ip_routes("").is_empty() && parse_ip_routes("garbage\n\n").is_empty());
    }

    #[test]
    fn rules_name_the_tables_and_the_ranges_they_select() {
        let (prefixes, tables) = parse_ip_rules(IP_RULES);
        assert_eq!(prefixes, [(u32::from(Ipv4Addr::new(10, 234, 0, 0)), 0xFFFF_0000)]);
        assert_eq!(tables, ["local", "main", "52", "77", "default"]);
        assert_eq!(parse_ip_rules(""), (Vec::new(), Vec::new()));
    }

    #[test]
    fn the_plan_avoids_a_range_only_a_policy_table_or_rule_routes() {
        // Nothing in main reaches 10.233.86.0/24 or 10.234.0.0/16: table 52 and a rule do.
        let host = view(&["ip"], true, 0)
            .file("/proc/net/route", ROUTES)
            .ip_output("-4 rule show", IP_RULES)
            .ip_output("-4 route show table all", IP_ROUTES);
        let plan = NetPlan::for_host(&host, 5557).expect("plan");
        let h = plan.veth.expect("addresses").host;
        // 10.233 block 5557 is 10.233.86.213, inside table 52's /24: the next free block is used.
        assert_eq!(h, Ipv4Addr::new(10, 233, 87, 1), "{h}");
        let main_only = view(&["ip"], true, 0).file("/proc/net/route", ROUTES);
        assert_eq!(NetPlan::for_host(&main_only, 5557).expect("plan").veth.expect("addresses").host, Ipv4Addr::new(10, 233, 86, 213));
        // The rule's range is avoided even with table 52 unreadable.
        let rule_only = view(&["ip"], true, 0).file("/proc/net/route", ROUTES).ip_output("-4 rule show", IP_RULES);
        let routes = host_routes(&rule_only);
        assert!(routes.contains(&(u32::from(Ipv4Addr::new(10, 234, 0, 0)), 0xFFFF_0000)), "{routes:?}");
    }

    #[test]
    fn an_exit_nodes_default_route_in_an_earlier_table_refuses_the_pair() {
        // Table 52 with a default route (Tailscale's exit node), looked up at 5270, before main at 32766.
        let exit = "default dev tailscale0 table 52\n100.100.100.100 dev tailscale0 table 52\n";
        let routes = format!("default via 192.168.0.1 dev eth0 proto dhcp metric 100\n{exit}");
        let host = view(&["ip"], true, 0)
            .file("/proc/net/route", ROUTES)
            .ip_output("-4 rule show", IP_RULES)
            .ip_output("-4 route show table all", &routes);
        let e = NetPlan::for_host(&host, 5557).expect_err("refused").to_string();
        assert!(e.contains("table 52") && e.contains("exit node") && e.contains("pasta or slirp4netns"), "{e}");
        // Read table by table, the same.
        let by_table = view(&["ip"], true, 0)
            .file("/proc/net/route", ROUTES)
            .ip_output("-4 rule show", IP_RULES)
            .ip_output("-4 route show table 52", exit);
        assert!(NetPlan::for_host(&by_table, 5557).is_err());
        // Pins: no default in table 52 (IP_ROUTES), a default in main only, a default in a table
        // looked up after main, a rule with a selector, a throw default: all plan.
        assert_eq!(early_default_tables(IP_RULES, IP_ROUTES, None), Vec::<String>::new());
        assert!(early_default_tables(IP_RULES, "default via 192.168.0.1 dev eth0\n", None).is_empty());
        assert!(early_default_tables(IP_RULES, "default dev x table default\n", None).is_empty());
        assert!(early_default_tables(IP_RULES, "default dev x table 77\n", None).is_empty(), "77 is selected by destination only");
        assert!(
            early_default_tables("100: from all fwmark 0x1 lookup 9\n32766: from all lookup main\n", "default dev x table 9\n", None)
                .is_empty()
        );
        assert!(early_default_tables(IP_RULES, "throw default table 52\n", None).is_empty());
        // A user-mode uplink never looks.
        let pasta = view(&["ip", "pasta"], true, 1000).ip_output("-4 rule show", IP_RULES).ip_output("-4 route show table all", &routes);
        assert_eq!(NetPlan::for_host(&pasta, 5557).expect("plan").veth, None);
    }

    #[test]
    fn without_a_table_all_listing_the_tables_the_rules_name_are_read_one_by_one() {
        let host = view(&["ip"], true, 0)
            .file("/proc/net/route", ROUTES)
            .ip_output("-4 rule show", IP_RULES)
            .ip_output("-4 route show table 52", "10.233.86.0/24 dev tun7 table 52\n");
        let routes = host_routes(&host);
        assert!(routes.contains(&(u32::from(Ipv4Addr::new(10, 233, 86, 0)), 0xFFFF_FF00)), "{routes:?}");
    }

    #[test]
    fn a_held_block_is_stepped_over_released_on_drop_and_a_dead_owners_is_taken_over() {
        let r = scratch_reservations();
        let (a, held_a) = r.claim(5557, &[]).expect("a");
        let (b, held_b) = r.claim(5557, &[]).expect("same port, next block");
        assert_ne!(a.host, b.host);
        assert_eq!(u32::from(b.host), u32::from(a.host) + 4, "the next /30 of the pool");
        drop(held_a);
        let (c, _held_c) = r.claim(5557, &[]).expect("a's block is free again");
        assert_eq!(c.host, a.host);
        // A claim left by a process that is gone (a crashed launcher) doesn't hold the block.
        let mut dead = sh("exit 0");
        let pid = dead.id();
        dead.wait().expect("reaped");
        let stale = r.dir.join(Ipv4Addr::from(u32::from(b.host) - 1 + 4).to_string());
        std::fs::write(&stale, format!("{pid}\n")).expect("stale claim");
        let (d, _held_d) = r.claim(5557, &[]).expect("takes over the stale block");
        assert_eq!(u32::from(d.host), u32::from(b.host) + 4);
        // A file with no pid in it is as dead.
        let junk = r.dir.join(Ipv4Addr::from(u32::from(d.host) - 1 + 4).to_string());
        std::fs::write(&junk, "not a pid").expect("junk claim");
        assert_eq!(u32::from(r.claim(5557, &[]).expect("junk").0.host), u32::from(d.host) + 4);
        drop(held_b);
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn a_claim_never_takes_a_routed_block() {
        let r = scratch_reservations();
        let routes = [(u32::from(Ipv4Addr::new(10, 233, 86, 212)), 0xFFFF_FFFC)];
        let (a, _h) = r.claim(5557, &routes).expect("claim");
        assert_eq!(a.host, Ipv4Addr::new(10, 233, 86, 217));
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[test]
    fn a_reservation_directory_that_cant_be_written_is_named_in_the_error() {
        let r = Reservations::new("/proc/simee-no-such/veth");
        let e = r.claim(5557, &[]).expect_err("unwritable").to_string();
        assert!(e.contains("/proc/simee-no-such/veth") && e.contains("can write"), "{e}");
    }

    #[test]
    fn plans_made_at_once_never_share_a_slash_30() {
        // Ports that collide modulo a /16's 16384 blocks, planned from many threads at the same
        // time: each plan holds its block, so all the subnets differ.
        let dir = std::env::temp_dir().join(format!("simee-resv-race-{}", std::process::id()));
        let r = Reservations::new(&dir);
        let plans: Vec<NetPlan> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..32u16)
                .map(|i| {
                    let r = r.clone();
                    // Four distinct ports, each eight times over.
                    s.spawn(move || NetPlan::new(Uplink::Veth, 5557 + (i % 4) * 16384, &[], &r).expect("plan"))
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("thread")).collect()
        });
        let mut hosts: Vec<_> = plans.iter().map(|p| p.veth.expect("addresses").host).collect();
        hosts.sort();
        hosts.dedup();
        assert_eq!(hosts.len(), 32, "every plan got its own /30");
        // A session made from a plan keeps its block after the plan itself is gone.
        let held = plans[0].reservation.clone().expect("held");
        let first = plans[0].veth.expect("addresses").host;
        drop(plans);
        let (again, again_held) = r.claim(5557, &[]).expect("claim");
        assert_ne!(again.host, first, "the block is still held by {held:?}");
        drop((held, again_held));
        let (freed, _h) = r.claim(5557, &[]).expect("claim");
        assert_eq!(freed.host, Ipv4Addr::new(10, 233, 86, 213), "everything released: the port's own block is free again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_forwarder_refuses_a_non_loopback_address() {
        let to: SocketAddr = "127.0.0.1:9".parse().expect("addr");
        for bad in ["0.0.0.0:0", "192.0.2.1:0", "[::]:0"] {
            let e = Forwarder::start(bad.parse().expect("addr"), to).err().unwrap_or_else(|| panic!("{bad} was accepted"));
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{bad}");
        }
    }

    #[test]
    fn the_forwarder_carries_a_connection_both_ways_and_stops_on_drop() {
        let upstream = TcpListener::bind("127.0.0.1:0").expect("upstream");
        let to = upstream.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut c, _) = upstream.accept().expect("accept");
            let mut buf = [0u8; 5];
            c.read_exact(&mut buf).expect("read");
            c.write_all(&buf.map(|b| b.to_ascii_uppercase())).expect("write");
        });
        let fwd = Forwarder::start("127.0.0.1:0".parse().expect("addr"), to).expect("forwarder");
        let listen = fwd.local_addr();
        assert!(listen.ip().is_loopback());
        let mut c = TcpStream::connect(listen).expect("connect");
        c.write_all(b"hello").expect("write");
        let mut back = [0u8; 5];
        c.read_exact(&mut back).expect("read");
        assert_eq!(&back, b"HELLO");
        server.join().expect("server");
        drop(fwd);
        // Nothing listens once the forwarder is dropped: the port can be bound again. (Connecting to
        // it would also succeed against any other test's listener that was handed the same port.)
        TcpListener::bind(listen).expect("the forwarder's port is free once it is dropped");
    }

    fn scratch_reservations() -> Reservations {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        Reservations::new(std::env::temp_dir().join(format!("simee-resv-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))))
    }

    fn sh(script: &str) -> Child {
        Command::new("sh").args(["-c", script]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("sh")
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("emu-container-net-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn a_daemon_is_ready_when_its_pid_file_has_a_pid() {
        let dir = tmp("ready");
        let pid_file = dir.join("p.pid");
        let mut child = sh(&format!("sleep 0.1; echo 42 > '{}'; sleep 30", pid_file.display()));
        let r = wait_ready(&mut child, &Ready::PidFile(pid_file), &dir.join("log"), Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();
        r.expect("ready");
    }

    #[test]
    fn a_daemon_that_exits_is_reported_with_its_log() {
        let dir = tmp("exit");
        std::fs::write(dir.join("log"), "Failed to open() /dev/net/tun: No such file or directory\n").expect("log");
        let mut child = sh("exit 3");
        let e = wait_ready(&mut child, &Ready::PidFile(dir.join("never")), &dir.join("log"), Duration::from_secs(10))
            .expect_err("exit")
            .to_string();
        assert!(e.contains("exited") && e.contains("/dev/net/tun"), "{e}");
    }

    #[test]
    fn a_daemon_that_never_says_ready_hits_the_deadline() {
        let dir = tmp("slow");
        let mut child = sh("sleep 30");
        let e = wait_ready(&mut child, &Ready::Socket(dir.join("nope.sock")), &dir.join("log"), Duration::from_millis(50))
            .expect_err("deadline")
            .to_string();
        let _ = child.kill();
        let _ = child.wait();
        assert!(e.contains("wasn't ready within"), "{e}");
    }

    #[test]
    fn the_api_call_needs_a_return_in_the_answer() {
        let dir = tmp("api");
        for (answer, ok) in [(r#"{"return":{"id":1}}"#, true), (r#"{"error":{"desc":"bad port"}}"#, false), ("not json", false)] {
            let socket = dir.join(format!("s{ok}-{}.sock", answer.len()));
            let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
            let reply = answer.to_string();
            let server = std::thread::spawn(move || {
                let (mut c, _) = listener.accept().expect("accept");
                let mut req = String::new();
                c.read_to_string(&mut req).expect("request");
                c.write_all(reply.as_bytes()).expect("reply");
                req
            });
            let r = api_call(&socket, r#"{"execute":"add_hostfwd"}"#);
            assert_eq!(server.join().expect("server").trim(), r#"{"execute":"add_hostfwd"}"#);
            assert_eq!(r.is_ok(), ok, "{answer}: {r:?}");
        }
    }
}
