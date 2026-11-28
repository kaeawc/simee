//! Starting Android's `/init` in a user namespace, Redroid-style, with no container runtime.
//!
//! [`plan`] turns what the device needs (the unpacked image, the display, the host's devices) into
//! data: the namespaces, the id mapping, the mounts in order, and `/init`'s arguments. Tests assert
//! that data. [`Plan::script`] renders it as the `sh` script that runs inside the namespaces, and
//! [`start`] runs `unshare` on it, maps the ids from outside, and lets the script go.
//!
//! What the plan reproduces is what Docker gives Redroid (docs/research/container-backend.md):
//! a root filesystem that is an overlay of the image (the writable layer is the device's own
//! directory), a `/dev` populated with the nodes `redroid.common.rc` chmods, binderfs mounted with
//! its three devices linked where older images look for them, `/proc`, and the host's `/sys`.
//! The container has a network namespace of its own (`crate::net`): its adbd is reached only
//! through `127.0.0.1:<adb_port>` on the host, never on the host's other addresses. Its root is a
//! subordinate id, never the launching user's own uid or the host's root ([`IdMap`]).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::net::{NetPlan, Session};
use crate::probe::{HostView, subordinate_range};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{what}: {err}")]
    Io { what: String, err: std::io::Error },
    #[error("mapping the container's ids: {0}")]
    Map(String),
    #[error("the container's network: {0}")]
    Net(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let what = what.into();
    move |err| Error::Io { what, err }
}

/// How the guest renders (`androidboot.redroid_gpu_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    /// Software rendering inside the container (SwiftShader): needs nothing from the host.
    Guest,
    /// The host's GPU through its render node.
    Host,
}

impl GpuMode {
    pub fn name(self) -> &'static str {
        match self {
            GpuMode::Guest => "guest",
            GpuMode::Host => "host",
        }
    }
}

/// What the display is (from the AVD's profile).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    /// Redroid's default (15 fps without a GPU) when `None`.
    pub fps: Option<u32>,
}

/// What a container needs to know about its device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The device's own state directory: `upper/`, `work/` and `merged/` live here.
    pub dir: PathBuf,
    /// The unpacked image, the overlay's read-only lower layer.
    pub rootfs: PathBuf,
    /// The image's entrypoint: `/init` and the arguments it was built to take.
    pub entrypoint: Vec<String>,
    pub display: Display,
    pub gpu: GpuMode,
    /// Extra `key=value` arguments for `/init`: Redroid turns `ro.xxx=value` into a property
    /// override and `androidboot.xxx` into `ro.boot.xxx`.
    pub props: Vec<String>,
    /// The host's loopback port that reaches the guest's adbd (`127.0.0.1:<adb_port>` only).
    pub adb_port: u16,
    pub adb_auth: AdbAuth,
}

/// Whether the guest's adbd asks for an authorized key. The pinned image ships with it off
/// (`ro.adb.secure=0`), which is safe only because the port is on loopback; with keys, other users
/// of the same host can't use the loopback port either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdbAuth {
    /// The image's setting: any process that can reach the loopback port gets a shell.
    Disabled,
    /// `ro.adb.secure=1`, and these public keys (`adbkey.pub` lines) in the guest's
    /// `/data/misc/adb/adb_keys`.
    Keys(Vec<String>),
}

/// Where the guest's adbd reads authorized keys.
pub const ADB_KEYS_PATH: &str = "data/misc/adb/adb_keys";

impl AdbAuth {
    /// The keys adb on this host presents: the lines of `<android_dir>/adbkey.pub` (`~/.android`,
    /// or `$ANDROID_USER_HOME`).
    pub fn host_keys(android_dir: &Path) -> Result<AdbAuth> {
        let path = android_dir.join("adbkey.pub");
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::Invalid(format!("{}: {e}: start the adb server once (`adb start-server`) so it creates its key", path.display()))
        })?;
        let keys: Vec<String> = text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
        if keys.is_empty() {
            return Err(Error::Invalid(format!("{} holds no key", path.display())));
        }
        Ok(AdbAuth::Keys(keys))
    }
}

/// How the container's ids map to the host's. Container root is never the host's root: a guest
/// that escaped its namespaces as host root could rewrite kernel settings (`core_pattern`,
/// `uevent_helper`) and own the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdMap {
    /// Run by host root, which writes the maps itself: container ids `0..count` are host ids
    /// `start..start+count`, a range of their own (root's `/etc/subuid` range, else
    /// [`DEDICATED_RANGE`]). The container's files are owned by that range.
    Root { uid: (u32, u32), gid: (u32, u32) },
    /// An unprivileged user: container ids `0..count` are the user's subordinate range, as
    /// `/etc/subuid` and `/etc/subgid` grant, written through `newuidmap` and `newgidmap`. The
    /// user's own uid (`uid`, `gid`) is *not* in the container's map: a guest that got out would
    /// be an unprivileged id of no use against the user's sockets and files (the simee server's
    /// socket, ssh-agent). It is in the admin map ([`IdMap::admin_actions`]), a short-lived
    /// namespace that runs only `chown` and `rm` for the container's directories.
    Rootless { uid: u32, gid: u32, sub_uid: (u32, u32), sub_gid: (u32, u32) },
}

/// The most ids mapped: Android's app uids run from 10000, and its shared gids reach 59999.
const MAPPED_IDS: u32 = 65536;

/// What root maps the container to when `/etc/subuid` gives root no range: above systemd-nspawn's
/// container ranges (which end at 1,878,982,656) and below 2^31, unused by any account.
pub const DEDICATED_RANGE: (u32, u32) = (2_000_000_000, MAPPED_IDS);

impl IdMap {
    /// The mapping for the process `view` describes (`gid` is its effective group): root gets its
    /// own subordinate range or the dedicated one; a user needs `/etc/subuid` and `/etc/subgid`
    /// ranges, and the error says how to get them.
    pub fn for_host(view: &dyn HostView, gid: u32) -> Result<IdMap> {
        let uid = view.euid();
        let range = |file: &str, who: u32| subordinate_range(view, file, who).filter(|&(_, n)| n >= MAPPED_IDS);
        if uid == 0 {
            return Ok(IdMap::Root {
                uid: range("/etc/subuid", 0).map_or(DEDICATED_RANGE, |(s, _)| (s, MAPPED_IDS)),
                gid: range("/etc/subgid", 0).map_or(DEDICATED_RANGE, |(s, _)| (s, MAPPED_IDS)),
            });
        }
        let need = |file: &str| {
            range(file, uid).ok_or_else(|| Error::Map(format!("{file} has no range of {MAPPED_IDS} ids for uid {uid}: `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER`")))
        };
        Ok(IdMap::Rootless { uid, gid, sub_uid: need("/etc/subuid")?, sub_gid: need("/etc/subgid")? })
    }

    /// The host ids that container root (and so every file the container creates as root) has:
    /// the directories the container writes in are handed to them first (`Plan::prepare_host`).
    /// Never the launching user's own ids, nor root's.
    pub fn container_root_owner(&self) -> (u32, u32) {
        match self {
            IdMap::Root { uid, gid } => (uid.0, gid.0),
            IdMap::Rootless { sub_uid, sub_gid, .. } => (sub_uid.0, sub_gid.0),
        }
    }

    /// What the parent does to give the namespace of process `pid` its ids.
    pub fn actions(&self, pid: u32) -> Vec<Action> {
        match self {
            IdMap::Root { uid, gid } => [("uid_map", uid), ("gid_map", gid)]
                .iter()
                .map(|(f, (start, count))| Action::Write {
                    path: PathBuf::from(format!("/proc/{pid}/{f}")),
                    text: format!("0 {start} {count}\n"),
                })
                .collect(),
            IdMap::Rootless { sub_uid, sub_gid, .. } => [("newuidmap", *sub_uid), ("newgidmap", *sub_gid)]
                .iter()
                .map(|&(tool, (start, _))| map_command(tool, pid, &[(0, start, MAPPED_IDS)]))
                .collect(),
        }
    }

    /// The admin namespace's maps: the user is root there (so it may `chown` to and delete files
    /// of the container's ids), and container id `n` is id `n + 1`. No guest code runs in it. Run
    /// by host root there is nothing to administer (root can do both directly): the container's
    /// own maps.
    pub fn admin_actions(&self, pid: u32) -> Vec<Action> {
        match self {
            IdMap::Root { .. } => self.actions(pid),
            IdMap::Rootless { uid, gid, sub_uid, sub_gid } => [("newuidmap", *uid, *sub_uid), ("newgidmap", *gid, *sub_gid)]
                .iter()
                .map(|&(tool, own, (start, _))| map_command(tool, pid, &[(0, own, 1), (1, start, MAPPED_IDS - 1)]))
                .collect(),
        }
    }
}

/// `newuidmap`/`newgidmap <pid> <inside> <outside> <count>…`.
fn map_command(tool: &str, pid: u32, ranges: &[(u32, u32, u32)]) -> Action {
    let mut argv = vec![tool.to_string(), pid.to_string()];
    for (inside, outside, count) in ranges {
        argv.extend([inside.to_string(), outside.to_string(), count.to_string()]);
    }
    Action::Run(argv)
}

/// The helper executable (`crate::helper`): `emu` itself under a hidden subcommand, or this
/// crate's `simee-container-helper` binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Helper {
    pub program: PathBuf,
    /// Arguments that make `program` act as the helper (`__container-helper` for `emu`).
    pub prefix: Vec<String>,
}

impl Helper {
    /// The command line for the helper request `request`.
    pub fn argv(&self, request: &[String]) -> Vec<String> {
        let mut argv = vec![self.program.display().to_string()];
        argv.extend(self.prefix.iter().cloned());
        argv.extend(request.iter().cloned());
        argv
    }
}

/// One step of the id mapping, as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Write { path: PathBuf, text: String },
    Run(Vec<String>),
}

/// A mount, as `mount(8)` would take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `-t`: a filesystem type, or `None` for a bind mount.
    pub fstype: Option<&'static str>,
    pub source: String,
    pub target: PathBuf,
    /// `-o`, comma-joined.
    pub options: Vec<String>,
    /// `--rbind` instead of `--bind` (a bind mount only).
    pub recursive: bool,
}

/// One step of the setup script, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Mkdir(PathBuf),
    /// An empty file to bind a device node onto.
    Touch(PathBuf),
    Mount(Mount),
    /// Remounts a bind mount read-only, keeping the flags a user namespace locks on it.
    ReadOnly(PathBuf),
    /// [`Step::ReadOnly`] for a mount and every mount under it, each with its own flags kept
    /// (what the host's `/sys` is made of, once recursively bound).
    ReadOnlyTree(PathBuf),
    Symlink {
        target: String,
        link: PathBuf,
    },
    /// A command run as written (`ip link set lo up`).
    Run(Vec<String>),
    /// A file with `content` and `mode`, replacing any there.
    Write {
        path: PathBuf,
        content: String,
        mode: u32,
    },
    /// Fails the setup with `message` unless `path` exists.
    Require {
        path: PathBuf,
        message: String,
    },
}

/// Everything needed to start a container, as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The overlay's merged directory: the container's root.
    pub root: PathBuf,
    /// The device's state directory (the overlay's `upper`, `work` and `merged` are under it).
    pub state_dir: PathBuf,
    pub idmap: IdMap,
    pub helper: Helper,
    pub steps: Vec<Step>,
    /// `/init` and its arguments, run as the container's pid 1.
    pub init: Vec<String>,
    /// The container's network namespace and how adb gets out of it.
    pub net: NetPlan,
}

/// The namespaces `unshare` creates, besides the network namespace a container adds
/// ([`unshare_command`]).
pub const NAMESPACES: [&str; 6] = ["--user", "--mount", "--pid", "--uts", "--ipc", "--cgroup"];

/// The nodes `redroid.common.rc` chmods, which a container runtime normally provides.
const DEVICE_NODES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

/// The three binder devices, and where older images look for them.
const BINDER_DEVICES: [&str; 3] = ["binder", "hwbinder", "vndbinder"];

fn check_path(what: &str, p: &Path) -> Result<()> {
    let s = p.to_str().ok_or_else(|| Error::Invalid(format!("{what} {} isn't UTF-8", p.display())))?;
    if !p.is_absolute() {
        return Err(Error::Invalid(format!("{what} {s} must be an absolute path")));
    }
    // `,` and `:` end an overlay option's value; NUL and newlines have no place in any argument.
    if s.chars().any(|c| matches!(c, ',' | ':' | '\0' | '\n' | ' ' | '\t' | '\\')) {
        return Err(Error::Invalid(format!(
            "{what} {s:?} contains a comma, colon, space, backslash, newline or NUL, which an overlay mount option or /proc/self/mountinfo can't carry"
        )));
    }
    Ok(())
}

fn check_prop(p: &str) -> Result<()> {
    let ok = p.split_once('=').is_some_and(|(k, _)| !k.is_empty()) && !p.chars().any(|c| c.is_whitespace() || c == '\0');
    if ok { Ok(()) } else { Err(Error::Invalid(format!("init argument {p:?} must be key=value with no whitespace"))) }
}

impl Spec {
    /// The arguments after `/init`: the image's, then Redroid's display, GPU and memfd settings,
    /// then the caller's.
    pub fn init_args(&self) -> Vec<String> {
        let mut args: Vec<String> = self.entrypoint.iter().skip(1).cloned().collect();
        args.push(format!("androidboot.redroid_width={}", self.display.width));
        args.push(format!("androidboot.redroid_height={}", self.display.height));
        args.push(format!("androidboot.redroid_dpi={}", self.display.dpi));
        if let Some(fps) = self.display.fps {
            args.push(format!("androidboot.redroid_fps={fps}"));
        }
        args.push(format!("androidboot.redroid_gpu_mode={}", self.gpu.name()));
        // ashmem left the kernel in 5.18; memfd replaces it.
        args.push("androidboot.use_memfd=true".into());
        if matches!(self.adb_auth, AdbAuth::Keys(_)) {
            args.push("ro.adb.secure=1".into());
        }
        args.extend(self.props.iter().cloned());
        args
    }

    fn validate(&self) -> Result<()> {
        check_path("the device directory", &self.dir)?;
        check_path("the root filesystem", &self.rootfs)?;
        if self.entrypoint.first().map(String::as_str) != Some("/init") {
            return Err(Error::Invalid(format!("the image's entrypoint must start with /init, not {:?}", self.entrypoint.first())));
        }
        for a in self.entrypoint.iter().skip(1).chain(&self.props) {
            check_prop(a)?;
        }
        if let AdbAuth::Keys(keys) = &self.adb_auth {
            if keys.is_empty() {
                return Err(Error::Invalid("adb authentication with no key would lock every client out".into()));
            }
            for k in keys {
                if k.trim().is_empty() || !k.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
                    return Err(Error::Invalid(format!("adb key {k:?} must be one line of printable text")));
                }
            }
        }
        if self.display.width == 0 || self.display.height == 0 || self.display.dpi == 0 {
            return Err(Error::Invalid(format!(
                "the display is {}x{} at {} dpi",
                self.display.width, self.display.height, self.display.dpi
            )));
        }
        Ok(())
    }
}

/// The plan for `spec` on the host `view` describes (which optional device nodes it has).
pub fn plan(spec: &Spec, view: &dyn HostView, idmap: IdMap, helper: Helper) -> Result<Plan> {
    spec.validate()?;
    let net = NetPlan::for_host(view, spec.adb_port)?;
    let root = spec.dir.join("merged");
    let at = |sub: &str| root.join(sub);
    let (upper, work) = (spec.dir.join("upper"), spec.dir.join("work"));
    let mount = |fstype: Option<&'static str>, source: &str, target: PathBuf, options: &[&str]| {
        Step::Mount(Mount {
            fstype,
            source: source.to_string(),
            target,
            options: options.iter().map(|s| s.to_string()).collect(),
            recursive: false,
        })
    };

    let mut steps = vec![Step::Mkdir(upper.clone()), Step::Mkdir(work.clone()), Step::Mkdir(root.clone())];

    // The root: the image, read-only, under the device's own writable layer. `userxattr` keeps the
    // overlay's bookkeeping in `user.*` attributes, which a user namespace may write.
    steps.push(Step::Mount(Mount {
        fstype: Some("overlay"),
        source: "overlay".into(),
        target: root.clone(),
        options: vec![
            format!("lowerdir={}", spec.rootfs.display()),
            format!("upperdir={}", upper.display()),
            format!("workdir={}", work.display()),
            "userxattr".into(),
        ],
        recursive: false,
    }));

    // The guest's adbd asks for these keys when `ro.adb.secure=1` (see `Spec::init_args`).
    if let AdbAuth::Keys(keys) = &spec.adb_auth {
        let file = at(ADB_KEYS_PATH);
        steps.push(Step::Mkdir(file.parent().expect("the keys file has a directory").to_path_buf()));
        steps.push(Step::Write { path: file, content: keys.iter().map(|k| format!("{k}\n")).collect(), mode: 0o644 });
    }

    // The container's own network namespace is empty (loopback only, and down) until the parent
    // attaches the uplink; this brings the guest's end up.
    steps.extend(net.guest_commands().into_iter().map(Step::Run));

    // /dev: a tmpfs holding the nodes Redroid's init expects to find (it only chmods them).
    steps.push(Step::Mkdir(at("dev")));
    steps.push(mount(Some("tmpfs"), "tmpfs", at("dev"), &["mode=755", "nosuid"]));
    for node in DEVICE_NODES {
        steps.push(Step::Touch(at(&format!("dev/{node}"))));
        steps.push(Step::Mount(Mount {
            fstype: None,
            source: format!("/dev/{node}"),
            target: at(&format!("dev/{node}")),
            options: vec![],
            recursive: false,
        }));
    }
    // Optional nodes: only what this host has.
    let mut optional = vec!["fuse", "ion"];
    if spec.gpu == GpuMode::Host {
        optional.push("dri");
    }
    optional.push("dma_heap");
    for node in optional {
        let host = format!("/dev/{node}");
        if !view.exists(&host) {
            continue;
        }
        let target = at(&format!("dev/{node}"));
        // A directory (dri, dma_heap) is a bind of a directory; a node needs a file to bind onto.
        steps.push(if matches!(node, "dri" | "dma_heap") { Step::Mkdir(target.clone()) } else { Step::Touch(target.clone()) });
        steps.push(Step::Mount(Mount {
            fstype: None,
            source: host,
            target,
            options: vec![],
            recursive: matches!(node, "dri" | "dma_heap"),
        }));
    }
    steps.push(Step::Mkdir(at("dev/pts")));
    steps.push(mount(Some("devpts"), "devpts", at("dev/pts"), &["newinstance", "ptmxmode=0666", "mode=0620"]));
    steps.push(Step::Symlink { target: "pts/ptmx".into(), link: at("dev/ptmx") });
    steps.push(Step::Mkdir(at("dev/shm")));
    steps.push(mount(Some("tmpfs"), "tmpfs", at("dev/shm"), &["mode=1777", "nosuid", "nodev"]));

    // binderfs, with its devices linked where older images look for them (Android's own init
    // mounts another instance over this one on newer images).
    steps.push(Step::Mkdir(at("dev/binderfs")));
    steps.push(mount(Some("binder"), "binder", at("dev/binderfs"), &[]));
    for dev in BINDER_DEVICES {
        steps.push(Step::Require {
            path: at(&format!("dev/binderfs/{dev}")),
            message: format!("binderfs came up without {dev}: load binder_linux with devices=binder,hwbinder,vndbinder (or boot a kernel whose CONFIG_ANDROID_BINDER_DEVICES lists them)"),
        });
        steps.push(Step::Symlink { target: format!("binderfs/{dev}"), link: at(&format!("dev/{dev}")) });
    }

    // /proc is this container's pid namespace, with the paths runc masks or makes read-only: they
    // expose the host kernel (kcore, the sysrq trigger) or tune it (/proc/sys).
    let proc = at("proc");
    steps.push(Step::Mkdir(proc.clone()));
    steps.push(mount(Some("proc"), "proc", proc.clone(), &["nosuid", "nodev", "noexec"]));
    for name in PROC_MASKED_FILES {
        if view.exists(&format!("/proc/{name}")) {
            steps.push(Step::Mount(Mount {
                fstype: None,
                source: "/dev/null".into(),
                target: proc.join(name),
                options: vec![],
                recursive: false,
            }));
        }
    }
    for name in PROC_MASKED_DIRS {
        if view.exists(&format!("/proc/{name}")) {
            steps.push(mount(Some("tmpfs"), "tmpfs", proc.join(name), &["ro", "nosuid", "nodev", "noexec", "size=0"]));
        }
    }
    for name in PROC_READ_ONLY {
        if view.exists(&format!("/proc/{name}")) {
            steps.push(Step::Mount(Mount {
                fstype: None,
                source: proc.join(name).display().to_string(),
                target: proc.join(name),
                options: vec![],
                recursive: false,
            }));
            steps.push(Step::ReadOnly(proc.join(name)));
        }
    }

    // /sys is the host's (a user namespace can't mount a sysfs without owning the network
    // namespace). It has to be bound recursively (the kernel refuses a plain bind of a mount with
    // locked children), then every mount in the tree is made read-only, and /sys/firmware masked.
    let sys = at("sys");
    steps.push(Step::Mkdir(sys.clone()));
    steps.push(Step::Mount(Mount { fstype: None, source: "/sys".into(), target: sys.clone(), options: vec![], recursive: true }));
    steps.push(Step::ReadOnlyTree(sys.clone()));
    if view.exists("/sys/firmware") {
        steps.push(mount(Some("tmpfs"), "tmpfs", sys.join("firmware"), &["ro", "nosuid", "nodev", "noexec", "size=0"]));
    }

    let mut init = vec!["/init".to_string()];
    init.extend(spec.init_args());
    Ok(Plan { root, state_dir: spec.dir.clone(), idmap, helper, steps, init, net })
}

/// Files under `/proc` that expose the host kernel: bound over with `/dev/null`.
const PROC_MASKED_FILES: [&str; 6] = ["kcore", "keys", "latency_stats", "timer_list", "timer_stats", "sched_debug"];
/// Directories under `/proc` hidden under an empty read-only tmpfs.
const PROC_MASKED_DIRS: [&str; 2] = ["acpi", "scsi"];
/// Paths under `/proc` that stay visible but can't be written.
const PROC_READ_ONLY: [&str; 5] = ["bus", "fs", "irq", "sys", "sysrq-trigger"];

/// POSIX-shell single quoting: safe for any bytes but NUL.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn mount_argv(m: &Mount) -> Vec<String> {
    let mut argv = vec!["mount".to_string()];
    match m.fstype {
        Some(t) => argv.extend(["-t".to_string(), t.to_string()]),
        None => argv.push(if m.recursive { "--rbind" } else { "--bind" }.to_string()),
    }
    if !m.options.is_empty() {
        argv.extend(["-o".to_string(), m.options.join(",")]);
    }
    argv.push(m.source.clone());
    argv.push(m.target.display().to_string());
    argv
}

/// `mount(8)` making a bind mount read-only. A mount copied into a user namespace has `nosuid`,
/// `nodev` and `noexec` locked on, and a remount that doesn't repeat them is refused.
fn read_only_argv(target: &Path) -> Vec<String> {
    ["mount", "-o", "remount,bind,ro,nosuid,nodev,noexec"].iter().map(|s| s.to_string()).chain([target.display().to_string()]).collect()
}

/// The script lines that remount `target` and every mount under it read-only. Each keeps the
/// per-mount flags `/proc/self/mountinfo` shows for it (a user namespace locks them on, and a
/// remount that drops one is refused); the mount points are matched literally, which is why paths
/// may hold no spaces (mountinfo escapes them).
fn read_only_tree_script(target: &Path) -> String {
    let t = quote(&target.display().to_string());
    format!(
        "while read -r _id _parent _dev _root mp opts _rest; do\n  case \"$mp\" in {t} | {t}/*)\n    o=${{opts#rw}}; o=${{o#,}}\n    step mount -o \"remount,bind,ro${{o:+,$o}}\" \"$mp\" ;;\n  esac\ndone < /proc/self/mountinfo"
    )
}

impl Plan {
    /// The `mount` command each mount step runs, in order: for tests and `-verbose`.
    pub fn mount_commands(&self) -> Vec<Vec<String>> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Mount(m) => Some(mount_argv(m)),
                Step::ReadOnly(t) => Some(read_only_argv(t)),
                _ => None,
            })
            .collect()
    }

    /// The setup as an `sh` script: waits for the parent to map the ids, does each step (naming
    /// the one that failed), and hands over to the helper, which pivots into the new root, drops
    /// the old one and becomes `/init`, the container's pid 1.
    pub fn script(&self) -> String {
        self.script_with(&self.init)
    }

    /// [`Plan::script`] ending in `argv` instead of `/init`, for exercising the mounts.
    pub fn script_with(&self, argv: &[String]) -> String {
        let mut s = String::from("set -eu\nstep() { \"$@\" || { echo \"emu: container setup failed: $*\" >&2; exit 1; }; }\n");
        s.push_str("# The parent writes the id maps, then a line.\nread -r _go\n");
        let words = |argv: &[String]| argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        for step in &self.steps {
            let line = match step {
                Step::Mkdir(p) => format!("step mkdir -p {}", quote(&p.display().to_string())),
                Step::Touch(p) => format!("step touch {}", quote(&p.display().to_string())),
                Step::Mount(m) => format!("step {}", words(&mount_argv(m))),
                Step::ReadOnly(t) => format!("step {}", words(&read_only_argv(t))),
                Step::ReadOnlyTree(t) => read_only_tree_script(t),
                Step::Symlink { target, link } => format!("step ln -sf {} {}", quote(target), quote(&link.display().to_string())),
                Step::Run(argv) => format!("step {}", words(argv)),
                Step::Write { path, content, mode } => {
                    let p = quote(&path.display().to_string());
                    format!(
                        "printf %s {} > {p} || {{ echo {} >&2; exit 1; }}\nstep chmod {mode:o} {p}",
                        quote(content),
                        quote(&format!("emu: container setup failed: writing {}", path.display()))
                    )
                }
                Step::Require { path, message } => format!(
                    "[ -e {} ] || {{ echo {} >&2; exit 1; }}",
                    quote(&path.display().to_string()),
                    quote(&format!("emu: container setup failed: {message}"))
                ),
            };
            let _ = writeln!(s, "{line}");
        }
        let mut request = vec!["pivot".to_string(), self.root.display().to_string(), "--".to_string()];
        request.extend(argv.iter().cloned());
        let _ = writeln!(s, "exec {}", words(&self.helper.argv(&request)));
        s
    }

    /// The `unshare` command line that runs [`Plan::script`] in fresh namespaces.
    pub fn command(&self) -> Result<Vec<String>> {
        unshare_command(&self.idmap, Some(&self.helper), &self.script(), true)
    }

    /// Creates the state directory and hands it to container root (see [`prepare_dir`]), so the
    /// container can create `upper`, `work` and `merged` in it.
    pub fn prepare_host(&self) -> Result<()> {
        // The container's process runs the helper too.
        check_searchable(&self.idmap, &self.helper.program)?;
        prepare_dir(&self.idmap, &self.state_dir)
    }
}

/// Fails when container root couldn't reach `path`: the container is another user (a range of its
/// own, never the launching user), and every directory above `path` must let others search it.
pub fn check_searchable(idmap: &IdMap, path: &Path) -> Result<()> {
    let (uid, _) = idmap.container_root_owner();
    for dir in path.ancestors().skip(1) {
        let mode = std::fs::metadata(dir).map_err(io(format!("reading {}", dir.display())))?.permissions();
        if std::os::unix::fs::PermissionsExt::mode(&mode) & 0o001 == 0 {
            return Err(Error::Invalid(format!(
                "{} isn't searchable by other users, so the container (host uid {uid}) can't reach {}: `chmod o+x {}`",
                dir.display(),
                path.display(),
                dir.display()
            )));
        }
    }
    Ok(())
}

/// Creates `dir` and hands it to container root: the container writes in it as an id range of its
/// own. Root `chown`s directly; an unprivileged user does it from the admin namespace
/// ([`IdMap::admin_actions`]), where container root is id 1.
pub fn prepare_dir(idmap: &IdMap, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(io(format!("creating {}", dir.display())))?;
    check_searchable(idmap, dir)?;
    match idmap {
        IdMap::Root { uid, gid } => {
            std::os::unix::fs::chown(dir, Some(uid.0), Some(gid.0)).map_err(io(format!("handing {} to container root", dir.display())))
        }
        IdMap::Rootless { .. } => {
            run_admin(idmap, &format!("exec chown 1:1 -- {}", quote(&dir.display().to_string())), "handing it to container root", dir)
        }
    }
}

/// `unshare` with the container's namespaces around `sh -c <script>`. `--fork` makes the script
/// pid 1 of the new pid namespace; `--kill-child` takes it down with `unshare`. `net` adds a
/// network namespace of its own (a container always has one; the id mapping's own helpers run
/// without).
///
/// For an unprivileged user the process `unshare` starts has an uid that isn't in the container's
/// map (container root is a subordinate id, not the user), so it would lose its capabilities at
/// `exec`: `--keep-caps` keeps them, and the helper's `become-root` waits for the maps and takes
/// the namespace's uid 0 before the script runs. That needs the `helper`.
pub fn unshare_command(idmap: &IdMap, helper: Option<&Helper>, script: &str, net: bool) -> Result<Vec<String>> {
    let mut argv = vec!["unshare".to_string()];
    argv.extend(NAMESPACES.iter().map(|s| s.to_string()));
    if net {
        argv.push("--net".into());
    }
    let rootless = matches!(idmap, IdMap::Rootless { .. });
    if rootless {
        argv.push("--keep-caps".into());
    }
    argv.extend(["--fork".into(), "--kill-child".into(), "--".into()]);
    if rootless {
        let helper =
            helper.ok_or_else(|| Error::Invalid("an unprivileged container needs the helper: it becomes the namespace's root".into()))?;
        argv.extend(helper.argv(&["become-root".to_string(), "--".to_string()]));
    }
    argv.extend(["sh".into(), "-c".into(), script.into(), "simee-container".into()]);
    Ok(argv)
}

/// `unshare` with only a user namespace, for the admin maps: `chown` and `rm` on the container's
/// files, as the launching user who is root there.
pub fn admin_command(script: &str) -> Vec<String> {
    ["unshare", "--user", "--fork", "--kill-child", "--", "sh", "-c", script, "simee-admin"].map(String::from).to_vec()
}

/// `argv` run under the helper's `die-with-parent`, so it is killed when the process `parent`
/// (this one) dies. With `unshare --kill-child` that takes the whole container down.
pub fn supervised(helper: &Helper, parent: u32, argv: &[String]) -> Vec<String> {
    let mut request = vec!["die-with-parent".to_string(), "--parent".to_string(), parent.to_string(), "--".to_string()];
    request.extend(argv.iter().cloned());
    helper.argv(&request)
}

/// How long to wait for `unshare` to create its user namespace before giving up. It takes
/// milliseconds; this only bounds a hung or never-started child.
pub const NAMESPACE_DEADLINE: Duration = Duration::from_secs(30);

/// Which of the id mapping's namespaces a process runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Container,
    Admin,
}

/// Runs `script` (which starts by reading a line from stdin: see [`Plan::script`]) in the
/// container's namespaces, maps its ids, and releases it. Returns the running `unshare`. With a
/// `helper` the child dies if this process does (the thread calling this must outlive the child:
/// see `crate::helper`). No network namespace: for the work that needs none (an unpack).
pub fn spawn_isolated(idmap: &IdMap, script: &str, helper: Option<&Helper>, stdout: Stdio, stderr: Stdio) -> Result<Child> {
    let argv = unshare_command(idmap, helper, script, false)?;
    spawn_in(idmap, Role::Container, argv, helper, stdout, stderr, |_| Ok(())).map(|(child, ())| child)
}

/// [`spawn_isolated`] with the network namespace of `plan`: once the namespaces exist and their
/// ids are mapped, the uplink is attached (`dir` is where its files go: somewhere the launching
/// user can write) and only then is the container released, so `/init` finds its `eth0`.
pub fn spawn_container(plan: &Plan, script: &str, dir: &Path, stdout: Stdio, stderr: Stdio) -> Result<(Child, Session)> {
    let argv = unshare_command(&plan.idmap, Some(&plan.helper), script, true)?;
    spawn_in(&plan.idmap, Role::Container, argv, Some(&plan.helper), stdout, stderr, |pid| {
        Session::start(&plan.net, pid, dir, Some(&plan.helper))
    })
}

/// Starts `argv` (an `unshare` command), maps its ids, runs `attach` with its pid, releases it.
fn spawn_in<T>(
    idmap: &IdMap,
    role: Role,
    mut argv: Vec<String>,
    helper: Option<&Helper>,
    stdout: Stdio,
    stderr: Stdio,
    attach: impl FnOnce(u32) -> Result<T>,
) -> Result<(Child, T)> {
    if let Some(h) = helper {
        argv = supervised(h, std::process::id(), &argv);
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir("/");
    // Run by host root, the container's process must itself be the user its root maps to: the one
    // that calls unshare(CLONE_NEWUSER) is the one that is root inside it, and host root's own
    // uid is not in the map (it would be nobody there, with no capabilities). So drop to the
    // container's range first. An unprivileged user can't change uid: its process keeps its
    // capabilities with `--keep-caps` and becomes root through the helper instead.
    if let (Role::Container, IdMap::Root { uid, gid }) = (role, idmap) {
        use std::os::unix::process::CommandExt;
        cmd.uid(uid.0).gid(gid.0);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|e| Error::Io { what: format!("starting {}", argv[0]), err: e })?;
    match map_and_release(idmap, role, &mut child, NAMESPACE_DEADLINE, attach) {
        Ok(attached) => Ok((child, attached)),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// `unshare` creates the user namespace in its own process, so its pid is the one to map. The
/// script waits on stdin until the maps are written and `attach` has run.
fn map_and_release<T>(
    idmap: &IdMap,
    role: Role,
    child: &mut Child,
    deadline: Duration,
    attach: impl FnOnce(u32) -> Result<T>,
) -> Result<T> {
    use std::io::Write;
    wait_for_namespace(child, deadline)?;
    let actions = if role == Role::Admin { idmap.admin_actions(child.id()) } else { idmap.actions(child.id()) };
    for action in actions {
        match action {
            Action::Write { path, text } => std::fs::write(&path, text).map_err(io(format!("writing {}", path.display())))?,
            Action::Run(argv) => {
                let out = Command::new(&argv[0]).args(&argv[1..]).output().map_err(io(format!("running {}", argv[0])))?;
                if !out.status.success() {
                    return Err(Error::Map(format!("`{}` failed: {}", argv.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
                }
            }
        }
    }
    let attached = attach(child.id())?;
    let stdin = child.stdin.as_mut().ok_or_else(|| Error::Map("no stdin to release the container with".into()))?;
    stdin.write_all(b"go\n").map_err(io("releasing the container"))?;
    // Closing stdin: the script reads one line and never again.
    drop(child.stdin.take());
    Ok(attached)
}

/// Waits until the child has made its user namespace (its `ns/user` link differs from ours), so
/// writing its maps can't race `unshare`'s own setup. It polls the kernel's answer every
/// millisecond and stops when the namespace exists, the child is gone, or `deadline` passes.
fn wait_for_namespace(child: &mut Child, deadline: Duration) -> Result<()> {
    let link = PathBuf::from(format!("/proc/{}/ns/user", child.id()));
    let ours = std::fs::read_link("/proc/self/ns/user").map_err(io("reading our user namespace"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(io("waiting for the container's process"))? {
            return Err(Error::Map(format!("it exited ({status}) before its user namespace could be mapped; its message is in the log")));
        }
        match std::fs::read_link(&link) {
            Ok(theirs) if theirs != ours => return Ok(()),
            _ => {}
        }
        if start.elapsed() >= deadline {
            return Err(Error::Map(format!("no user namespace appeared for pid {} within {deadline:?}", child.id())));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Runs `script` in the admin namespace of an unprivileged user (see [`IdMap::admin_actions`]);
/// errors carry what the script said. `what` and `path` describe it for the message.
fn run_admin(idmap: &IdMap, script: &str, what: &str, path: &Path) -> Result<()> {
    let script = format!("set -eu\nread -r _go\n{script}\n");
    let (child, ()) = spawn_in(idmap, Role::Admin, admin_command(&script), None, Stdio::null(), Stdio::piped(), |_| Ok(()))?;
    let out = child.wait_with_output().map_err(io("waiting for the admin namespace's command"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Io {
        what: format!("{what} ({}) from inside the user namespace", path.display()),
        err: std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    })
}

/// Deletes `path` from inside the container's user namespace, where the files the container made
/// are owned by ids the host user can't touch (subordinate ids). Errors are returned, with what
/// `rm` said.
pub fn remove_tree(idmap: &IdMap, helper: Option<&Helper>, path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    let rm = format!("exec rm -rf -- {}", quote(&path.display().to_string()));
    match idmap {
        IdMap::Rootless { .. } => run_admin(idmap, &rm, "removing it", path)?,
        IdMap::Root { .. } => {
            let script = format!("set -eu\nread -r _go\n{rm}\n");
            let out =
                spawn_isolated(idmap, &script, helper, Stdio::null(), Stdio::piped())?.wait_with_output().map_err(io("waiting for rm"))?;
            if !out.status.success() {
                return Err(Error::Io {
                    what: format!("removing {} from inside the user namespace", path.display()),
                    err: std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()),
                });
            }
        }
    }
    match path.try_exists() {
        Ok(false) => Ok(()),
        _ => Err(Error::Invalid(format!("{} is still there after rm -rf", path.display()))),
    }
}

/// A running container. Dropping it kills it.
pub struct Container {
    child: Child,
    /// The network helpers and the loopback forward; dropped (and stopped) after the child.
    net: Session,
}

impl Container {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether `/init` is still running.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Whether the container's network helpers are still running.
    pub fn network_healthy(&mut self) -> bool {
        self.net.healthy()
    }

    /// Waits for the container to exit.
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// Stops the container hard: `unshare --kill-child` takes the pid namespace's init and
    /// everything under it down. (Android shuts down gracefully through `adb reboot -p`.)
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()?;
        self.child.wait().map(drop)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.running() {
            let _ = self.kill();
        }
    }
}

/// Starts `/init` for `plan`, its console and boot output going to `log` (the network helpers'
/// files go next to it, which must be a directory the launching user can write). Call it from a
/// thread that lives as long as the container: the kernel's parent-death signal follows the thread
/// that spawned the child (`crate::helper`).
pub fn start(plan: &Plan, log: &Path) -> Result<Container> {
    plan.prepare_host()?;
    let dir = log.parent().ok_or_else(|| Error::Invalid(format!("the log {} has no directory", log.display())))?;
    let file = std::fs::File::create(log).map_err(io(format!("creating {}", log.display())))?;
    let err = file.try_clone().map_err(io("duplicating the log handle"))?;
    let (child, net) = spawn_container(plan, &plan.script(), dir, Stdio::from(file), Stdio::from(err))?;
    Ok(Container { child, net })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeView;

    fn spec() -> Spec {
        Spec {
            dir: "/avd/Pixel.avd/simee/container".into(),
            rootfs: "/cache/redroid-15-x86_64/rootfs".into(),
            entrypoint: ["/init", "qemu=1", "androidboot.hardware=redroid"].map(String::from).to_vec(),
            display: Display { width: 720, height: 1280, dpi: 320, fps: None },
            gpu: GpuMode::Guest,
            props: vec![],
            adb_port: 5557,
            adb_auth: AdbAuth::Disabled,
        }
    }

    fn host() -> FakeView {
        FakeView::container_host()
    }

    fn helper() -> Helper {
        Helper { program: "/usr/bin/emu".into(), prefix: vec!["__container-helper".into()] }
    }

    fn root_map() -> IdMap {
        IdMap::Root { uid: DEDICATED_RANGE, gid: DEDICATED_RANGE }
    }

    fn plan_of(spec: &Spec, view: &FakeView) -> Plan {
        plan(spec, view, root_map(), helper()).expect("plan")
    }

    #[test]
    fn init_gets_the_images_arguments_then_redroids_settings() {
        let args = spec().init_args();
        assert_eq!(
            args,
            [
                "qemu=1",
                "androidboot.hardware=redroid",
                "androidboot.redroid_width=720",
                "androidboot.redroid_height=1280",
                "androidboot.redroid_dpi=320",
                "androidboot.redroid_gpu_mode=guest",
                "androidboot.use_memfd=true",
            ]
        );
        assert_eq!(plan_of(&spec(), &host()).init[0], "/init");
    }

    #[test]
    fn fps_gpu_mode_and_caller_props_come_through() {
        let mut s = spec();
        s.display.fps = Some(30);
        s.gpu = GpuMode::Host;
        s.props = vec!["ro.secure=0".into(), "androidboot.redroid_net_ndns=1".into()];
        let args = s.init_args();
        assert!(args.contains(&"androidboot.redroid_fps=30".to_string()));
        assert!(args.contains(&"androidboot.redroid_gpu_mode=host".to_string()));
        assert_eq!(args[args.len() - 2..], ["ro.secure=0", "androidboot.redroid_net_ndns=1"]);
        assert_eq!(args.iter().filter(|a| a.starts_with("androidboot.use_memfd")).count(), 1, "memfd is always on");
    }

    #[test]
    fn the_overlay_is_the_image_under_the_devices_own_layer() {
        let p = plan_of(&spec(), &host());
        assert_eq!(p.root, PathBuf::from("/avd/Pixel.avd/simee/container/merged"));
        assert_eq!(
            p.mount_commands()[0],
            [
                "mount",
                "-t",
                "overlay",
                "-o",
                "lowerdir=/cache/redroid-15-x86_64/rootfs,upperdir=/avd/Pixel.avd/simee/container/upper,workdir=/avd/Pixel.avd/simee/container/work,userxattr",
                "overlay",
                "/avd/Pixel.avd/simee/container/merged",
            ]
        );
        // The writable directories exist before the overlay is mounted.
        let first_mount = p.steps.iter().position(|s| matches!(s, Step::Mount(_))).expect("mount");
        let made: Vec<_> =
            p.steps[..first_mount].iter().filter_map(|s| if let Step::Mkdir(d) = s { Some(d.clone()) } else { None }).collect();
        assert_eq!(made.len(), 3);
        assert!(
            made.iter().any(|d| d.ends_with("upper"))
                && made.iter().any(|d| d.ends_with("work"))
                && made.iter().any(|d| d.ends_with("merged"))
        );
    }

    /// A mount command as a short word: `overlay /`, `bind /dev/null -> /dev/null`, `ro /sys`.
    fn describe(c: &[String]) -> String {
        let strip = |p: &str| match p.trim_start_matches("/avd/Pixel.avd/simee/container/merged") {
            "" => "/".to_string(),
            rest => rest.to_string(),
        };
        let target = strip(c.last().expect("target"));
        if let Some(i) = c.iter().position(|a| a == "-t") {
            return format!("{} {target}", c[i + 1]);
        }
        if c[1] == "-o" {
            return format!("ro {target}");
        }
        format!("{} {} -> {target}", c[1].trim_start_matches('-'), strip(&c[c.len() - 2]))
    }

    #[test]
    fn the_mounts_in_order() {
        let p = plan_of(&spec(), &host());
        let kinds: Vec<String> = p.mount_commands().iter().map(|c| describe(c)).collect();
        assert_eq!(
            kinds,
            [
                "overlay /",
                "tmpfs /dev",
                "bind /dev/null -> /dev/null",
                "bind /dev/zero -> /dev/zero",
                "bind /dev/full -> /dev/full",
                "bind /dev/random -> /dev/random",
                "bind /dev/urandom -> /dev/urandom",
                "bind /dev/tty -> /dev/tty",
                "rbind /dev/dma_heap -> /dev/dma_heap",
                "devpts /dev/pts",
                "tmpfs /dev/shm",
                "binder /dev/binderfs",
                "proc /proc",
                "bind /dev/null -> /proc/kcore",
                "bind /dev/null -> /proc/keys",
                "bind /dev/null -> /proc/timer_list",
                "bind /dev/null -> /proc/sched_debug",
                "tmpfs /proc/acpi",
                "tmpfs /proc/scsi",
                "bind /proc/bus -> /proc/bus",
                "ro /proc/bus",
                "bind /proc/fs -> /proc/fs",
                "ro /proc/fs",
                "bind /proc/irq -> /proc/irq",
                "ro /proc/irq",
                "bind /proc/sys -> /proc/sys",
                "ro /proc/sys",
                "bind /proc/sysrq-trigger -> /proc/sysrq-trigger",
                "ro /proc/sysrq-trigger",
                "rbind /sys -> /sys",
                "tmpfs /sys/firmware",
            ],
        );
    }

    #[test]
    fn the_hosts_sys_is_bound_recursively_then_every_mount_in_it_is_made_read_only() {
        let p = plan_of(&spec(), &host());
        let sys = p.root.join("sys");
        let bind = p
            .steps
            .iter()
            .find_map(|s| {
                if let Step::Mount(m) = s
                    && m.source == "/sys"
                {
                    Some(m.clone())
                } else {
                    None
                }
            })
            .expect("sys bind");
        assert!(bind.recursive, "the kernel refuses a plain bind of a mount with locked children");
        let at = |f: &dyn Fn(&Step) -> bool| p.steps.iter().position(f).expect("step");
        let mounted = at(&|s| matches!(s, Step::Mount(m) if m.target == sys));
        let ro = at(&|s| matches!(s, Step::ReadOnlyTree(t) if *t == sys));
        assert_eq!(ro, mounted + 1, "made read-only straight away");
        let script = p.script();
        // Each mount under /sys keeps its own flags: the loop reads them from mountinfo.
        assert!(script.contains("while read -r _id _parent _dev _root mp opts _rest; do"), "{script}");
        assert!(
            script.contains("'/avd/Pixel.avd/simee/container/merged/sys' | '/avd/Pixel.avd/simee/container/merged/sys'/*)"),
            "{script}"
        );
        assert!(script.contains(r#"step mount -o "remount,bind,ro${o:+,$o}" "$mp""#), "{script}");
        assert!(script.contains("done < /proc/self/mountinfo"));
        assert_eq!(read_only_argv(&sys), ["mount", "-o", "remount,bind,ro,nosuid,nodev,noexec", &sys.display().to_string()]);
    }

    #[test]
    fn paths_mountinfo_would_escape_are_refused() {
        for dir in ["/with space/dev", "/with\ttab", "/back\\slash"] {
            let mut s = spec();
            s.dir = dir.into();
            assert!(plan(&s, &host(), root_map(), helper()).is_err(), "{dir:?}");
        }
    }

    #[test]
    fn proc_paths_that_expose_or_tune_the_host_are_masked_or_read_only() {
        let p = plan_of(&spec(), &host());
        let cmds = p.mount_commands();
        let root = p.root.display().to_string();
        let masked = |name: &str| {
            cmds.iter().any(|c| c[1] == "--bind" && c[c.len() - 2] == "/dev/null" && *c.last().expect("t") == format!("{root}/proc/{name}"))
        };
        for name in ["kcore", "keys", "timer_list", "sched_debug"] {
            assert!(masked(name), "{name}");
        }
        for name in ["bus", "fs", "irq", "sys", "sysrq-trigger"] {
            assert!(p.steps.contains(&Step::ReadOnly(p.root.join("proc").join(name))), "/proc/{name} read-only");
        }
        // The read-only remount follows its own bind mount.
        let ro = p.steps.iter().position(|s| *s == Step::ReadOnly(p.root.join("proc/sys"))).expect("ro");
        assert!(matches!(&p.steps[ro - 1], Step::Mount(m) if m.target == p.root.join("proc/sys") && m.fstype.is_none()));
    }

    #[test]
    fn masks_cover_only_what_the_host_has() {
        let mut bare = host();
        for gone in ["/proc/kcore", "/proc/scsi", "/proc/sysrq-trigger", "/sys/firmware"] {
            bare.paths.remove(gone);
        }
        let p = plan_of(&spec(), &bare);
        let cmds = p.mount_commands();
        assert!(!cmds.iter().any(|c| c.last().is_some_and(|t| t.ends_with("/proc/kcore")
            || t.ends_with("/proc/scsi")
            || t.ends_with("/proc/sysrq-trigger")
            || t.ends_with("/sys/firmware"))));
        assert!(cmds.iter().any(|c| c.last().is_some_and(|t| t.ends_with("/proc/keys"))), "the others stay");
    }

    #[test]
    fn binderfs_is_mounted_checked_and_linked_where_older_images_look() {
        let p = plan_of(&spec(), &host());
        let root = &p.root;
        let binder = p.mount_commands().into_iter().find(|c| c.contains(&"binder".to_string())).expect("binderfs");
        assert_eq!(binder, ["mount", "-t", "binder", "binder", &root.join("dev/binderfs").display().to_string()]);
        for dev in ["binder", "hwbinder", "vndbinder"] {
            let required = p.steps.iter().any(|s| matches!(s, Step::Require { path, message } if *path == root.join("dev/binderfs").join(dev) && message.contains("binder_linux")));
            let linked = p.steps.contains(&Step::Symlink { target: format!("binderfs/{dev}"), link: root.join("dev").join(dev) });
            assert!(required && linked, "{dev}");
        }
        // The devices are checked after the mount and before the links they would dangle from.
        let at = |f: &dyn Fn(&Step) -> bool| p.steps.iter().position(f).expect("step");
        let mount = at(&|s| matches!(s, Step::Mount(m) if m.fstype == Some("binder")));
        let require = at(&|s| matches!(s, Step::Require { .. }));
        assert!(mount < require);
    }

    #[test]
    fn dev_has_the_nodes_redroid_chmods_and_ptmx_through_a_private_devpts() {
        let p = plan_of(&spec(), &host());
        for node in ["null", "zero", "full", "random", "urandom", "tty"] {
            assert!(p.steps.contains(&Step::Touch(p.root.join("dev").join(node))), "{node}");
        }
        assert!(p.mount_commands().iter().any(|c| c.contains(&"newinstance,ptmxmode=0666,mode=0620".to_string())));
        assert!(p.steps.contains(&Step::Symlink { target: "pts/ptmx".into(), link: p.root.join("dev/ptmx") }));
    }

    #[test]
    fn optional_device_nodes_are_bound_only_when_the_host_has_them() {
        let bound = |p: &Plan, n: &str| p.mount_commands().iter().any(|c| c[c.len() - 2] == format!("/dev/{n}"));
        let bare = FakeView::container_host();
        let mut none = bare.clone();
        none.paths.clear();
        let p = plan_of(&spec(), &none);
        for n in ["fuse", "ion", "dri", "dma_heap"] {
            assert!(!bound(&p, n), "{n}");
        }
        let rich = bare.path("/dev/fuse").path("/dev/ion").path("/dev/dri");
        let p = plan_of(&spec(), &rich);
        assert!(bound(&p, "fuse") && bound(&p, "ion") && bound(&p, "dma_heap"));
        assert!(!bound(&p, "dri"), "the render node is for GPU mode host only");
        let mut host_gpu = spec();
        host_gpu.gpu = GpuMode::Host;
        let p = plan_of(&host_gpu, &rich);
        assert!(bound(&p, "dri"));
        // A directory is bound recursively; a node is bound onto a file.
        let dri = p
            .steps
            .iter()
            .find_map(|s| {
                if let Step::Mount(m) = s
                    && m.source == "/dev/dri"
                {
                    Some(m.clone())
                } else {
                    None
                }
            })
            .expect("dri mount");
        assert!(dri.recursive);
        assert!(p.steps.contains(&Step::Mkdir(p.root.join("dev/dri"))));
        assert!(p.steps.contains(&Step::Touch(p.root.join("dev/fuse"))));
    }

    #[test]
    fn the_container_has_namespaces_of_its_own_including_the_network() {
        for ns in ["--user", "--mount", "--pid", "--uts", "--ipc", "--cgroup"] {
            assert!(NAMESPACES.contains(&ns), "{ns}");
        }
        let cmd = plan_of(&spec(), &host()).command().expect("command");
        assert_eq!(cmd[0], "unshare");
        assert_eq!(cmd.iter().filter(|a| *a == "--net").count(), 1, "a network namespace of its own: {cmd:?}");
        assert!(cmd.contains(&"--fork".to_string()) && cmd.contains(&"--kill-child".to_string()));
        assert_eq!(cmd[cmd.len() - 4..cmd.len() - 2], ["sh", "-c"]);
        assert!(!cmd.contains(&"--keep-caps".to_string()), "root maps its own range and needs no help: {cmd:?}");
        // The id mapping's own helpers (an unpack, a `chown`) need no network.
        let bare = unshare_command(&root_map(), None, "true", false).expect("command");
        assert!(!bare.contains(&"--net".to_string()), "{bare:?}");
    }

    #[test]
    fn the_guests_loopback_comes_up_before_init_and_a_veth_guest_is_configured() {
        let steps = |p: &Plan| -> Vec<String> {
            p.steps.iter().filter_map(|s| if let Step::Run(a) = s { Some(a.join(" ")) } else { None }).collect()
        };
        let veth = plan_of(&spec(), &host());
        assert_eq!(veth.net.uplink, crate::net::Uplink::Veth, "root without pasta");
        let g = steps(&veth);
        assert_eq!(g[0], "ip link set lo up");
        assert!(g[1].starts_with("ip addr add 100.64.") && g[1].ends_with("/30 dev eth0"), "{g:?}");
        assert_eq!(g[2], "ip link set eth0 up");
        let script = veth.script();
        assert!(script.contains("step 'ip' 'link' 'set' 'lo' 'up'"), "{script}");
        assert!(script.find("'lo' 'up'").expect("lo") < script.find("exec ").expect("exec"));
        // With pasta the tool configures the guest's interface from outside.
        let user_mode = plan(&spec(), &host().tool("pasta"), root_map(), helper()).expect("plan");
        assert_eq!(user_mode.net.uplink, crate::net::Uplink::Pasta);
        assert_eq!(steps(&user_mode), ["ip link set lo up"]);
        assert_eq!(user_mode.net.adb_port, 5557);
    }

    #[test]
    fn a_host_that_cant_give_the_container_a_network_namespace_is_refused_with_the_fix() {
        let mut v = host().euid(1000);
        v.tools.remove("ip");
        let e = plan(&spec(), &v, root_map(), helper()).expect_err("no network").to_string();
        assert!(e.contains("fix: sudo apt install iproute2"), "{e}");
        let v = host().euid(1000);
        let e = plan(&spec(), &v, root_map(), helper()).expect_err("no tool").to_string();
        assert!(e.contains("neither pasta nor slirp4netns") && e.contains("passt"), "{e}");
        let mut s = spec();
        s.adb_port = 0;
        assert!(plan(&s, &host(), root_map(), helper()).expect_err("port 0").to_string().contains("not 0"));
    }

    #[test]
    fn adb_authentication_is_off_in_the_image_and_on_with_keys() {
        // Disabled: the image's own setting, nothing added.
        let off = plan_of(&spec(), &host());
        assert!(!off.init.iter().any(|a| a.starts_with("ro.adb.secure")), "{:?}", off.init);
        assert!(!off.steps.iter().any(|s| matches!(s, Step::Write { .. })));
        // Keys: the property, and the keys where adbd reads them, before the container starts.
        let mut s = spec();
        s.adb_auth = AdbAuth::Keys(vec!["QAAAA host-key user@host".into(), "QBBBB other".into()]);
        let on = plan_of(&s, &host());
        assert!(on.init.contains(&"ro.adb.secure=1".to_string()), "{:?}", on.init);
        let file = on.root.join("data/misc/adb/adb_keys");
        assert!(on.steps.contains(&Step::Write {
            path: file.clone(),
            content: "QAAAA host-key user@host\nQBBBB other\n".into(),
            mode: 0o644
        }));
        assert!(on.steps.contains(&Step::Mkdir(file.parent().expect("dir").to_path_buf())));
        let script = on.script();
        assert!(script.contains("printf %s 'QAAAA host-key user@host\nQBBBB other\n' > '"), "{script}");
        assert!(script.contains(&format!("step chmod 644 '{}'", file.display())), "{script}");
        assert!(script.find("adb_keys").expect("keys") < script.find("exec ").expect("exec"));
    }

    #[test]
    fn adb_keys_that_would_lock_everyone_out_or_break_the_file_are_refused() {
        let bad = |keys: Vec<String>| {
            let mut s = spec();
            s.adb_auth = AdbAuth::Keys(keys);
            plan(&s, &host(), root_map(), helper()).expect_err("refused").to_string()
        };
        assert!(bad(vec![]).contains("lock every client out"));
        assert!(bad(vec!["two\nlines".into()]).contains("one line"));
        assert!(bad(vec!["   ".into()]).contains("one line"));
        assert!(bad(vec!["nul\0".into()]).contains("one line"));
    }

    #[test]
    fn the_hosts_adb_keys_are_read_from_its_android_directory() {
        let dir = tmp("adbkeys");
        let e = AdbAuth::host_keys(&dir).expect_err("no key yet").to_string();
        assert!(e.contains("adb start-server"), "{e}");
        std::fs::write(dir.join("adbkey.pub"), "QAAAA key user@host\n\n").expect("write");
        assert_eq!(AdbAuth::host_keys(&dir).expect("keys"), AdbAuth::Keys(vec!["QAAAA key user@host".into()]));
        std::fs::write(dir.join("adbkey.pub"), "\n").expect("write");
        assert!(AdbAuth::host_keys(&dir).expect_err("empty").to_string().contains("no key"));
    }

    #[test]
    fn the_script_waits_for_the_id_maps_then_sets_up_then_execs_init() {
        let p = plan_of(&spec(), &host());
        let script = p.script();
        let read = script.find("read -r _go").expect("waits for the maps");
        let first_step = script.find("step mkdir").expect("steps");
        assert!(read < first_step, "nothing runs before the ids are mapped");
        let last = script.lines().last().expect("exec line");
        assert_eq!(
            last,
            "exec '/usr/bin/emu' '__container-helper' 'pivot' '/avd/Pixel.avd/simee/container/merged' '--' '/init' 'qemu=1' 'androidboot.hardware=redroid' 'androidboot.redroid_width=720' 'androidboot.redroid_height=1280' 'androidboot.redroid_dpi=320' 'androidboot.redroid_gpu_mode=guest' 'androidboot.use_memfd=true'"
        );
        assert!(script.contains("step 'mount' '-t' 'overlay' '-o' 'lowerdir=/cache/redroid-15-x86_64/rootfs,"), "{script}");
        assert!(script.contains("[ -e '/avd/Pixel.avd/simee/container/merged/dev/binderfs/hwbinder' ] ||"), "{script}");
        assert!(script.starts_with("set -eu\n"));
    }

    #[test]
    fn script_with_swaps_only_the_final_command() {
        let p = plan_of(&spec(), &host());
        let s = p.script_with(&["/bin/sh".into(), "-c".into(), "echo hi".into()]);
        assert!(s.ends_with("'pivot' '/avd/Pixel.avd/simee/container/merged' '--' '/bin/sh' '-c' 'echo hi'\n"), "{s}");
        assert_eq!(s.lines().count(), p.script().lines().count());
    }

    #[test]
    fn quoting_survives_quotes_spaces_and_dollars() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's $HOME `x`"), r"'it'\''s $HOME `x`'");
        // A prop value can't smuggle a command into the script: it is one quoted word.
        let mut s = spec();
        s.props = vec!["ro.x=a;touch/tmp/pwned".into()];
        let script = plan_of(&s, &host()).script();
        assert!(script.contains("'ro.x=a;touch/tmp/pwned'"), "{script}");
    }

    #[test]
    fn bad_specs_are_refused_before_anything_is_planned() {
        let bad = |f: &dyn Fn(&mut Spec)| {
            let mut s = spec();
            f(&mut s);
            plan(&s, &host(), root_map(), helper()).expect_err("refused").to_string()
        };
        assert!(bad(&|s| s.dir = "relative/dir".into()).contains("absolute"));
        assert!(bad(&|s| s.rootfs = "/has,comma/rootfs".into()).contains("comma"));
        assert!(bad(&|s| s.dir = "/has:colon".into()).contains("colon"));
        assert!(bad(&|s| s.dir = "/new\nline".into()).contains("newline"));
        assert!(bad(&|s| s.props = vec!["noequals".into()]).contains("key=value"));
        assert!(bad(&|s| s.props = vec!["a b=c".into()]).contains("whitespace"));
        assert!(bad(&|s| s.props = vec!["=x".into()]).contains("key=value"));
        assert!(bad(&|s| s.entrypoint = vec!["/bin/sh".into()]).contains("/init"));
        assert!(bad(&|s| s.entrypoint = vec![]).contains("/init"));
        assert!(bad(&|s| s.display.width = 0).contains("0x1280"));
    }

    #[test]
    fn root_maps_the_container_to_a_range_of_its_own_never_to_root() {
        let map = IdMap::for_host(&host(), 0).expect("root");
        assert_eq!(map, IdMap::Root { uid: DEDICATED_RANGE, gid: DEDICATED_RANGE });
        let actions = map.actions(4242);
        assert_eq!(
            actions,
            [
                Action::Write { path: "/proc/4242/uid_map".into(), text: "0 2000000000 65536\n".into() },
                Action::Write { path: "/proc/4242/gid_map".into(), text: "0 2000000000 65536\n".into() },
            ]
        );
        for a in &actions {
            let Action::Write { text, .. } = a else { panic!("root writes the maps itself") };
            let host_start: u32 = text.split_whitespace().nth(1).expect("outer").parse().expect("number");
            assert_ne!(host_start, 0, "container root must not be host root: {text}");
            assert!(!text.starts_with("0 0 "), "{text}");
        }
        assert_eq!(map.container_root_owner(), (2_000_000_000, 2_000_000_000));
        // Nothing maps the whole id space onto itself any more.
        assert!(DEDICATED_RANGE.0 > 1_878_982_656, "above systemd-nspawn's container ranges");
        assert!(u64::from(DEDICATED_RANGE.0) + u64::from(DEDICATED_RANGE.1) < 1 << 31);
    }

    #[test]
    fn root_prefers_its_own_subordinate_range_when_it_has_one() {
        let v = host()
            .file("/etc/passwd", "root:x:0:0::/root:/bin/sh\n")
            .file("/etc/subuid", "root:524288:131072\n")
            .file("/etc/subgid", "0:600000:65536\n");
        assert_eq!(IdMap::for_host(&v, 0).expect("root"), IdMap::Root { uid: (524288, 65536), gid: (600000, 65536) });
        // A range too small for Android is ignored for the dedicated one.
        let small = host().file("/etc/subuid", "0:524288:1000\n");
        assert_eq!(IdMap::for_host(&small, 0).expect("root"), IdMap::Root { uid: DEDICATED_RANGE, gid: DEDICATED_RANGE });
    }

    #[test]
    fn an_unprivileged_users_container_root_is_a_subordinate_id_never_their_own() {
        let map = IdMap::Rootless { uid: 1000, gid: 1001, sub_uid: (100000, 65536), sub_gid: (200000, 65536) };
        assert_eq!(map.container_root_owner(), (100000, 200000), "the first subordinate ids");
        assert_eq!(
            map.actions(7),
            [
                Action::Run(["newuidmap", "7", "0", "100000", "65536"].map(String::from).to_vec()),
                Action::Run(["newgidmap", "7", "0", "200000", "65536"].map(String::from).to_vec()),
            ]
        );
        // Whatever the user's ids and ranges, the container's map never reaches the user's own.
        for (uid, gid, sub) in [(1000, 1000, 100000), (1000, 1000, 165536), (0, 0, 100000), (65534, 65534, 1_000_000)] {
            let map = IdMap::Rootless { uid, gid, sub_uid: (sub, 65536), sub_gid: (sub, 65536) };
            for (action, own) in map.actions(1).iter().zip([uid, gid]) {
                let Action::Run(argv) = action else { panic!("{action:?}") };
                let ranges: Vec<u32> = argv[2..].iter().map(|n| n.parse().expect("number")).collect();
                for r in ranges.chunks(3) {
                    let (outside, count) = (r[1], r[2]);
                    assert!(!(outside..outside + count).contains(&own), "{argv:?} maps the user's own id {own}");
                }
            }
        }
    }

    #[test]
    fn the_admin_namespace_makes_the_user_root_and_the_containers_ids_start_at_1() {
        let map = IdMap::Rootless { uid: 1000, gid: 1001, sub_uid: (100000, 65536), sub_gid: (200000, 65536) };
        assert_eq!(
            map.admin_actions(7),
            [
                Action::Run(["newuidmap", "7", "0", "1000", "1", "1", "100000", "65535"].map(String::from).to_vec()),
                Action::Run(["newgidmap", "7", "0", "1001", "1", "1", "200000", "65535"].map(String::from).to_vec()),
            ]
        );
        // Container root is admin id 1, which is what `prepare_dir` chowns to; no guest code runs
        // there.
        let cmd = admin_command("exec chown 1:1 -- /d");
        assert_eq!(cmd[..2], ["unshare", "--user"]);
        assert!(!cmd.iter().any(|a| a == "--net" || a == "--keep-caps" || a == "--mount"), "{cmd:?}");
        // For root the admin and container maps are the same: there is nothing to administer.
        assert_eq!(root_map().admin_actions(7), root_map().actions(7));
    }

    #[test]
    fn an_unprivileged_container_keeps_its_capabilities_and_becomes_root_through_the_helper() {
        let map = IdMap::Rootless { uid: 1000, gid: 1000, sub_uid: (100000, 65536), sub_gid: (100000, 65536) };
        let cmd = unshare_command(&map, Some(&helper()), "true", true).expect("command");
        let at = |flag: &str| cmd.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag} missing: {cmd:?}"));
        assert!(at("--user") < at("--keep-caps") && at("--keep-caps") < at("--fork"), "--keep-caps comes with --user: {cmd:?}");
        let dashdash = at("--");
        assert_eq!(cmd[dashdash + 1..dashdash + 5], ["/usr/bin/emu", "__container-helper", "become-root", "--"]);
        assert_eq!(cmd[dashdash + 5..dashdash + 7], ["sh", "-c"]);
        assert_eq!(
            crate::helper::parse(&cmd[dashdash + 3..]),
            Ok(crate::helper::Request::BecomeRoot { command: vec!["sh".into(), "-c".into(), "true".into(), "simee-container".into()] })
        );
        // Without the helper there is nothing to become root.
        let e = unshare_command(&map, None, "true", true).expect_err("needs the helper").to_string();
        assert!(e.contains("needs the helper"), "{e}");
    }

    #[test]
    fn id_maps_are_derived_from_the_host() {
        assert!(matches!(IdMap::for_host(&host(), 0).expect("root"), IdMap::Root { .. }));
        let user = host()
            .euid(1000)
            .file("/etc/passwd", "ci:x:1000:1000::/home/ci:/bin/sh\n")
            .file("/etc/subuid", "ci:100000:65536\n")
            .file("/etc/subgid", "ci:300000:70000\n");
        assert_eq!(
            IdMap::for_host(&user, 1000).expect("rootless"),
            IdMap::Rootless { uid: 1000, gid: 1000, sub_uid: (100000, 65536), sub_gid: (300000, 70000) }
        );
        let short = user.clone().file("/etc/subuid", "ci:100000:1000\n");
        assert!(IdMap::for_host(&short, 1000).expect_err("too few").to_string().contains("usermod --add-subuids"));
        let none = host().euid(1000);
        assert!(matches!(IdMap::for_host(&none, 1000), Err(Error::Map(_))));
    }

    #[test]
    fn the_script_pivots_through_the_helper_and_never_chroots() {
        let p = plan_of(&spec(), &host());
        let script = p.script();
        assert!(!script.contains("chroot"), "a chroot leaves the host's root reachable");
        let request = vec!["pivot".to_string(), p.root.display().to_string(), "--".into(), "/init".into()];
        let argv = p.helper.argv(&request);
        assert_eq!(argv[..2], ["/usr/bin/emu", "__container-helper"]);
        // What the script hands the helper is what the helper parses.
        assert_eq!(
            crate::helper::parse(&argv[2..]),
            Ok(crate::helper::Request::Pivot { root: p.root.display().to_string(), init: vec!["/init".into()] })
        );
        assert_eq!(script.lines().last().expect("exec").split(' ').nth(3), Some("'pivot'"));
    }

    #[test]
    fn the_container_is_started_under_die_with_parent() {
        let p = plan_of(&spec(), &host());
        let argv = supervised(&p.helper, 77, &p.command().expect("command"));
        assert_eq!(argv[..6], ["/usr/bin/emu", "__container-helper", "die-with-parent", "--parent", "77", "--"]);
        assert_eq!(argv[6], "unshare");
        assert_eq!(
            crate::helper::parse(&argv[2..]).map(|r| matches!(r, crate::helper::Request::DieWithParent { parent: 77, .. })),
            Ok(true)
        );
    }

    fn tmp(tag: &str) -> PathBuf {
        crate::image::fixture::tmp(tag)
    }

    #[test]
    fn prepare_host_refuses_a_directory_container_root_cant_reach_for_either_kind_of_user() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmp("prepare");
        let mut p = plan_of(&spec(), &host());
        p.state_dir = base.join("a/b");
        // The container's root is never the launching user, so a private ancestor stops both: the
        // user's home directory is often 0750.
        for idmap in [root_map(), IdMap::Rootless { uid: 1000, gid: 1000, sub_uid: (100000, 65536), sub_gid: (100000, 65536) }] {
            p.idmap = idmap;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).expect("chmod");
            let e = p.prepare_host().expect_err("unsearchable").to_string();
            assert!(e.contains(&base.display().to_string()), "names the private directory: {e}");
            assert!(e.contains("isn't searchable") && e.contains("chmod o+x"), "{e}");
        }
        assert!(p.state_dir.is_dir(), "the directory is created before the check");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Namespaces are Linux's (`/proc/<pid>/ns/user`).
    #[cfg(target_os = "linux")]
    #[test]
    fn waiting_for_the_namespace_is_bounded_and_notices_a_dead_child() {
        // A child that never makes a namespace: the deadline, not a hang.
        let mut sleeper = Command::new("sleep").arg("60").spawn().expect("sleep");
        let e = wait_for_namespace(&mut sleeper, Duration::ZERO).expect_err("deadline").to_string();
        assert!(e.contains("no user namespace appeared"), "{e}");
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        // A child that is already gone is reported as such, whatever the deadline.
        let mut gone = Command::new("true").spawn().expect("true");
        let _ = gone.wait();
        let e = wait_for_namespace(&mut gone, Duration::from_secs(30)).expect_err("exited").to_string();
        assert!(e.contains("before its user namespace could be mapped"), "{e}");
    }

    #[test]
    fn dropping_a_container_kills_it() {
        // Signal 0 asks the kernel whether the process exists: Ok while it runs, ESRCH once it is
        // gone (and, since `Drop` waits for it, reaped rather than a zombie).
        let alive = |pid: u32| rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid as i32).expect("pid")).is_ok();
        let child = Command::new("sleep").arg("600").spawn().expect("sleep");
        let pid = child.id();
        let c = Container { child, net: Session::default() };
        assert!(alive(pid));
        drop(c);
        assert!(!alive(pid));
    }

    #[test]
    fn remove_tree_handles_a_missing_path_and_an_ordinary_tree() {
        let base = tmp("remove");
        remove_tree(&root_map(), None, &base.join("never-existed")).expect("missing is fine");
        std::fs::create_dir_all(base.join("t/u")).expect("mkdir");
        std::fs::write(base.join("t/u/f"), "x").expect("write");
        remove_tree(&root_map(), None, &base.join("t")).expect("removes");
        assert!(!base.join("t").exists());
    }
}
