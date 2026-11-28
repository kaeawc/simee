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
//!   a /30 from 100.64.0.0/10 per container, with a proxy thread listening on the loopback port.
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

/// The pair's addresses for a container whose adb port is `adb_port`: block number `adb_port` of
/// the /30s in 100.64.0.0/10 (shared address space, which no LAN uses), so two containers on one
/// host never share a subnet (their adb ports differ).
pub fn veth_addrs(adb_port: u16) -> VethAddrs {
    let base = u32::from(Ipv4Addr::new(100, 64, 0, 0)) + 4 * u32::from(adb_port);
    VethAddrs { host: Ipv4Addr::from(base + 1), guest: Ipv4Addr::from(base + 2) }
}

/// What a container's network is, as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPlan {
    pub uplink: Uplink,
    /// The host's loopback port that reaches the guest's adbd.
    pub adb_port: u16,
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
    pub fn new(uplink: Uplink, adb_port: u16) -> Result<NetPlan> {
        if adb_port == 0 {
            return Err(Error::Invalid("the adb port must be a loopback port, not 0".into()));
        }
        Ok(NetPlan { uplink, adb_port })
    }

    /// The plan for the host `view` describes; the error says what to install.
    pub fn for_host(view: &dyn HostView, adb_port: u16) -> Result<NetPlan> {
        let uplink = Uplink::choose(view).map_err(|m| Error::Net(format!("{} (fix: {})", m.why, m.fix)))?;
        NetPlan::new(uplink, adb_port)
    }

    /// The commands the container's setup script runs in its own namespace, before `/init`.
    pub fn guest_commands(&self) -> Vec<Vec<String>> {
        let ip = |args: &[&str]| ["ip"].iter().chain(args).map(|s| s.to_string()).collect::<Vec<_>>();
        let mut cmds = vec![ip(&["link", "set", "lo", "up"])];
        if self.uplink == Uplink::Veth {
            let a = veth_addrs(self.adb_port);
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
                let a = veth_addrs(self.adb_port);
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
}

impl Session {
    /// Wires the namespaces of process `pid` up as `plan` says. With a `helper` the daemon dies
    /// with this process (see [`supervised`]).
    pub fn start(plan: &NetPlan, pid: u32, dir: &Path, helper: Option<&Helper>) -> Result<Session> {
        let attach = plan.attach(pid, dir);
        // Any error returns early and drops `session`, which stops what already started.
        let mut session = Session::default();
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
        assert!(NetPlan::new(Uplink::Pasta, 0).is_err());
        assert_eq!(NetPlan::new(Uplink::Pasta, 5555).expect("plan").adb_port, 5555);
    }

    #[test]
    fn pasta_forwards_the_loopback_port_and_nothing_else() {
        let dir = Path::new("/run/avd");
        let a = NetPlan::new(Uplink::Pasta, 5557).expect("plan").attach(4242, dir);
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
        let a = NetPlan::new(Uplink::Slirp4netns, 5557).expect("plan").attach(4242, dir);
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
        let plan = NetPlan::new(Uplink::Veth, 5557).expect("plan");
        let a = plan.attach(4242, Path::new("/run/avd"));
        let a_s: Vec<String> = a.setup.iter().map(|c| c.join(" ")).collect();
        assert_eq!(a_s[0], "ip link add sveth5557 type veth peer name eth0 netns 4242");
        assert!(a_s[1].starts_with("ip addr add 100.64.") && a_s[1].ends_with("/30 dev sveth5557"), "{a_s:?}");
        assert_eq!(a_s[2], "ip link set sveth5557 up");
        let addrs = veth_addrs(5557);
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
            let g = NetPlan::new(uplink, 5557).expect("plan").guest_commands();
            assert_eq!(g, vec![["ip", "link", "set", "lo", "up"].map(String::from).to_vec()], "{uplink:?}");
        }
    }

    #[test]
    fn every_uplink_publishes_only_on_loopback() {
        for uplink in [Uplink::Pasta, Uplink::Slirp4netns, Uplink::Veth] {
            let a = NetPlan::new(uplink, 40000).expect("plan").attach(1, Path::new("/d"));
            let text = format!("{a:?}");
            assert!(!text.contains("0.0.0.0") && !text.contains("::"), "{uplink:?}: {text}");
            assert!(text.contains("127.0.0.1"), "{uplink:?} names the loopback address: {text}");
        }
    }

    #[test]
    fn veth_subnets_are_distinct_slash_30s_in_shared_address_space() {
        let (a, b) = (veth_addrs(5557), veth_addrs(5559));
        assert_ne!(a.host, b.host);
        for p in [1u16, 5555, 5557, 40000, 65535] {
            let v = veth_addrs(p);
            let (h, g) = (u32::from(v.host), u32::from(v.guest));
            assert_eq!(g, h + 1);
            assert_eq!(h & 3, 1, "host is .1 of its /30");
            assert_eq!(h >> 22, u32::from(Ipv4Addr::new(100, 64, 0, 0)) >> 22, "{p}: inside 100.64.0.0/10");
        }
        assert_eq!(veth_addrs(5557).host, Ipv4Addr::new(100, 64, 86, 213));
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
        assert!(TcpStream::connect(listen).is_err(), "nothing listens once the forwarder is dropped");
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
