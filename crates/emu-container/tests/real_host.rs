#![cfg(unix)]
//! The launcher against the real kernel. Each test needs `unshare`, `sh`, `ldd` and a kernel that
//! lets this user mount an overlay inside a user namespace, and returns at once unless
//! `SIMEE_E2E_CONTAINER=1`. Run as root and as an unprivileged user with a `/etc/subuid` range.
//!
//! On a host without binder the mount sequence stops at binderfs, and that message is the
//! assertion there; the rest runs with the binder steps filtered out.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use simee_emu_container::launch::{self, AdbAuth, Display, GpuMode, Helper, IdMap, Plan, Spec, Step};
use simee_emu_container::probe::{FsView, HostView, probe};

fn enabled() -> bool {
    let on = std::env::var_os("SIMEE_E2E_CONTAINER").is_some();
    if !on {
        eprintln!("skipped: set SIMEE_E2E_CONTAINER=1 to run the launcher against this host's kernel");
    }
    on
}

fn helper() -> Helper {
    Helper { program: PathBuf::from(env!("CARGO_BIN_EXE_simee-container-helper")), prefix: vec![] }
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("emu-container-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

/// Copies `bin` and the libraries it links into `rootfs`, at the same paths.
fn copy_with_libs(rootfs: &Path, bin: &Path) {
    let ldd = Command::new("ldd").arg(bin).output().expect("ldd");
    let mut files = vec![bin.to_path_buf()];
    for l in String::from_utf8_lossy(&ldd.stdout).lines() {
        if let Some(p) = l.split_whitespace().find(|w| w.starts_with('/')) {
            files.push(PathBuf::from(p));
        }
    }
    for f in files {
        let dest = rootfs.join(f.strip_prefix("/").expect("absolute"));
        std::fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
        std::fs::copy(&f, &dest).expect("copy");
    }
}

/// A root filesystem holding only the host's shell and `sleep` and the libraries they link, so a
/// script can run after the pivot. Returns the shell's path.
fn shell_rootfs(rootfs: &Path) -> String {
    let sh = std::fs::canonicalize("/bin/sh").expect("sh");
    copy_with_libs(rootfs, &sh);
    copy_with_libs(rootfs, &std::fs::canonicalize("/bin/sleep").expect("sleep"));
    sh.display().to_string()
}

/// A loopback port nothing listens on (for the adb forward).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

/// The real host, except that it claims pasta and `/dev/net/tun`: for planning a container whose
/// network is never attached (the id mapping and mounts don't need it).
struct PretendNet(FsView);

impl HostView for PretendNet {
    fn read(&self, path: &str) -> Option<String> {
        self.0.read(path)
    }
    fn exists(&self, path: &str) -> bool {
        path == "/dev/net/tun" || self.0.exists(path)
    }
    fn on_path(&self, name: &str) -> bool {
        name == "pasta" || self.0.on_path(name)
    }
    fn memfd_create(&self) -> bool {
        self.0.memfd_create()
    }
    fn euid(&self) -> u32 {
        self.0.euid()
    }
    fn egid(&self) -> u32 {
        self.0.egid()
    }
    fn arch(&self) -> String {
        self.0.arch()
    }
    fn binderfs_devices(&self) -> Option<Vec<String>> {
        self.0.binderfs_devices()
    }
}

/// The plan for this host and user, and whether its network can really be attached here. An
/// unprivileged user with no pasta or slirp4netns, or no `/dev/net/tun`, can't (said on stderr);
/// the plan is then made as if it could, for the tests that never start its network.
fn plan_in(dir: &Path, rootfs: PathBuf) -> (Plan, IdMap, bool) {
    let idmap = IdMap::for_host(&FsView, rustix::process::getegid().as_raw()).expect("an id mapping for this user");
    let spec = Spec {
        dir: dir.join("device"),
        rootfs,
        entrypoint: ["/init", "qemu=1", "androidboot.hardware=redroid"].map(String::from).to_vec(),
        display: Display { width: 720, height: 1280, dpi: 320, fps: None },
        gpu: GpuMode::Guest,
        props: vec![],
        adb_port: free_port(),
        adb_auth: AdbAuth::Disabled,
    };
    match launch::plan(&spec, &FsView, idmap.clone(), helper()) {
        Ok(p) => (p, idmap, true),
        Err(launch::Error::Net(why)) => {
            eprintln!("no network namespace for the container on this host ({why}): its network isn't attached in these tests");
            (launch::plan(&spec, &PretendNet(FsView), idmap.clone(), helper()).expect("plan"), idmap, false)
        }
        Err(e) => panic!("plan: {e}"),
    }
}

/// Where the network helpers' files go: next to the state directory, which belongs to container
/// root.
fn run_dir(p: &Plan) -> &Path {
    p.state_dir.parent().expect("a parent")
}

/// `p` without the binderfs steps, for a host with no binder: the rest of the setup still runs.
fn without_binder(p: &Plan) -> Plan {
    let mut rest = p.clone();
    rest.steps.retain(|s| match s {
        Step::Mount(m) => m.fstype != Some("binder"),
        Step::Require { .. } => false,
        Step::Symlink { link, .. } => !link.to_string_lossy().contains("binder"),
        _ => true,
    });
    rest
}

/// What a script run in the container's namespaces (with its network attached) prints.
fn run(p: &Plan, argv: &[String], net: bool) -> (bool, String, String) {
    p.prepare_host().expect("state dir");
    let mut p = p.clone();
    if !net {
        // No network namespace of its own: the plan's loopback and `eth0` commands would act on
        // the host's.
        p.steps.retain(|s| !matches!(s, Step::Run(_)));
    }
    let p = &p;
    let script = p.script_with(argv);
    // With the network namespace when this host can attach one; without otherwise (the script
    // doesn't need it).
    let (child, _net) = if net {
        let (c, n) = launch::spawn_container(p, &script, run_dir(p), Stdio::piped(), Stdio::piped()).expect("spawn");
        (c, Some(n))
    } else {
        (launch::spawn_isolated(&p.idmap, &script, Some(&p.helper), Stdio::piped(), Stdio::piped()).expect("spawn"), None)
    };
    // Hold the control pipe open until the container is done, as `Container` does: `pivot` treats a
    // pipe with no writer as a dead launcher, and `wait_with_output` would close it at once.
    let mut child = child;
    let control = child.stdin.take();
    let out = child.wait_with_output().expect("wait");
    drop(control);
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Shell builtins only: the container has nothing else.
const INSPECT: &str = r#"
echo started
while read -r l; do echo "MI $l"; done < /proc/self/mountinfo
while read -r l; do echo "UID $l"; done < /proc/self/uid_map
[ -e /etc/hostname ] && echo LEAK_ETC
[ -e /proc/1/root/etc/hostname ] && echo LEAK_PROC_ROOT
[ -s /proc/kcore ] && echo KCORE_VISIBLE
[ -e /dev/null ] && echo devnull
[ -L /dev/ptmx ] && echo ptmx
while read -r k v; do case "$k" in Uid:|Gid:|Groups:) echo "PROC $k $v";; esac; done < /proc/self/status
"#;

#[test]
fn real_host_mounts_pivots_and_leaves_no_path_to_the_host() {
    if !enabled() {
        return;
    }
    let tmp = tmp("mounts");
    let rootfs = tmp.join("rootfs");
    let sh = shell_rootfs(&rootfs);
    let (p, idmap, net) = plan_in(&tmp, rootfs);
    // The state directory belongs to container root, whose range the maps give.
    let argv = vec![sh.clone(), "-c".into(), INSPECT.into()];
    let binder = probe(&FsView).get(simee_emu_container::probe::Feature::Binder).is_some_and(|c| c.ok());
    let (ok, stdout, stderr) = run(&p, &argv, net);
    eprintln!("binder on this host: {binder}\nstdout: {stdout}\nstderr: {stderr}");
    let (stdout, stderr) = if binder {
        assert!(ok, "{stderr}");
        (stdout, stderr)
    } else {
        assert!(!ok);
        assert!(
            stderr.contains("emu: container setup failed: mount -t binder binder"),
            "a host without binder stops at binderfs, naming it: {stderr}"
        );
        let (ok, stdout, stderr) = run(&without_binder(&p), &argv, net);
        eprintln!("without binderfs:\nstdout: {stdout}\nstderr: {stderr}");
        assert!(ok, "{stderr}");
        (stdout, stderr)
    };
    assert!(stderr.is_empty() || !stderr.contains("emu: container setup failed"), "{stderr}");
    for want in ["started", "devnull", "ptmx"] {
        assert!(stdout.contains(want), "{want} missing: {stdout}");
    }

    // Nothing of the host's root is left: the root mount is the overlay, no host device backs
    // any mount, and the host's files can't be reached by path or through /proc.
    for leak in ["LEAK_ETC", "LEAK_PROC_ROOT", "KCORE_VISIBLE"] {
        assert!(!stdout.contains(leak), "{leak}: {stdout}");
    }
    let mounts: Vec<&str> = stdout.lines().filter_map(|l| l.strip_prefix("MI ")).collect();
    let mountpoint = |l: &str| l.split_whitespace().nth(4).unwrap_or("").to_string();
    let root_line = mounts.iter().find(|l| mountpoint(l) == "/").expect("a root mount");
    assert!(root_line.contains(" - overlay overlay "), "the root is the overlay: {root_line}");
    assert_eq!(mounts.iter().filter(|l| mountpoint(l) == "/").count(), 1, "the old root isn't stacked underneath: {mounts:#?}");
    assert!(!mounts.iter().any(|l| l.contains("/dev/root") || l.contains("/dev/vda")), "no host block device: {mounts:#?}");

    // /sys and the sensitive /proc paths are read-only.
    let opts = |path: &str| mounts.iter().find(|l| mountpoint(l) == path).map(|l| l.split_whitespace().nth(5).unwrap_or("").to_string());
    for path in ["/sys", "/proc/sys", "/proc/sysrq-trigger", "/proc/bus"] {
        if let Some(o) = opts(path) {
            assert!(o.split(',').any(|f| f == "ro"), "{path} should be read-only: {o}");
        }
    }

    // Container root is not host root.
    let map: Vec<Vec<&str>> = stdout.lines().filter_map(|l| l.strip_prefix("UID ")).map(|l| l.split_whitespace().collect()).collect();
    let first = &map[0];
    assert_eq!(first[0], "0");
    match idmap {
        IdMap::Root { uid, .. } => assert_eq!((first[1], first[2]), (uid.0.to_string().as_str(), uid.1.to_string().as_str())),
        // An unprivileged user's container root is the first subordinate id, and no line of the
        // map reaches the user's own uid.
        IdMap::Rootless { uid, sub_uid, .. } => {
            assert_eq!((first[1], first[2]), (sub_uid.0.to_string().as_str(), sub_uid.1.to_string().as_str()));
            for line in &map {
                let (outside, count): (u32, u32) = (line[1].parse().expect("outside"), line[2].parse().expect("count"));
                assert!(!(outside..outside + count).contains(&uid), "the map reaches the invoking uid {uid}: {map:?}");
            }
        }
    }
    assert_ne!(first[1], "0", "the container's root is not the host's root: {map:?}");
    // The process itself is that root: uid 0 and gid 0 inside, with no supplementary groups of the
    // host user carried in.
    let proc_line =
        |k: &str| stdout.lines().find_map(|l| l.strip_prefix(&format!("PROC {k}"))).map(|v| v.split_whitespace().collect::<Vec<_>>());
    assert_eq!(proc_line("Uid:"), Some(vec!["0"; 4]), "{stdout}");
    assert_eq!(proc_line("Gid:"), Some(vec!["0"; 4]), "{stdout}");
    assert_eq!(proc_line("Groups:"), Some(vec![]), "no host groups: {stdout}");
}

/// The listening TCP sockets on `port` in this namespace's `/proc/net/tcp{,6}`, as the local
/// address (hex) of each.
fn listeners_on(port: u16) -> Vec<String> {
    let mut found = vec![];
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for l in std::fs::read_to_string(file).unwrap_or_default().lines().skip(1) {
            let f: Vec<&str> = l.split_whitespace().collect();
            let Some((addr, p)) = f.get(1).and_then(|a| a.rsplit_once(':')) else { continue };
            if f.get(3) == Some(&"0A") && u16::from_str_radix(p, 16) == Ok(port) {
                found.push(format!("{file} {addr}"));
            }
        }
    }
    found
}

#[test]
fn real_host_the_container_has_its_own_network_and_adb_only_on_loopback() {
    if !enabled() {
        return;
    }
    let tmp = tmp("net");
    let rootfs = tmp.join("rootfs");
    let sh = shell_rootfs(&rootfs);
    let (p, _, net) = plan_in(&tmp, rootfs);
    if !net {
        return;
    }
    let p = if probe(&FsView).get(simee_emu_container::probe::Feature::Binder).is_some_and(|c| c.ok()) { p } else { without_binder(&p) };
    let port = p.net.adb_port;
    p.prepare_host().expect("state dir");
    // Print the guest's interfaces, then stay up so the host side can be looked at.
    let script = "echo started; while read -r l; do echo NET $l; done < /proc/net/dev; echo NETDONE; exec sleep 60";
    let (mut child, net) =
        launch::spawn_container(&p, &p.script_with(&[sh, "-c".into(), script.into()]), run_dir(&p), Stdio::piped(), Stdio::piped())
            .expect("spawn with its network");
    let mut out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
    let mut guest = vec![];
    loop {
        let mut line = String::new();
        if std::io::BufRead::read_line(&mut out, &mut line).expect("line") == 0 || line.trim() == "NETDONE" {
            break;
        }
        guest.push(line.trim().to_string());
    }
    eprintln!("uplink {:?}, guest says: {guest:#?}", p.net.uplink);

    // The container's namespace is its own.
    let theirs = std::fs::read_link(format!("/proc/{}/ns/net", child.id())).expect("their netns");
    assert_ne!(theirs, std::fs::read_link("/proc/self/ns/net").expect("our netns"), "the container shares the host's network namespace");
    // Its interfaces are loopback and the uplink's `eth0`, none of the host's.
    let ifaces: Vec<&str> =
        guest.iter().filter_map(|l| l.strip_prefix("NET ")).filter_map(|l| l.split_once(':')).map(|(name, _)| name.trim()).collect();
    assert_eq!(ifaces, ["lo", "eth0"], "the guest sees its own interfaces and none of the host's");

    // On the host, the adb port listens on loopback and nowhere else.
    let host = listeners_on(port);
    assert!(!host.is_empty(), "nothing listens on 127.0.0.1:{port}");
    for l in &host {
        assert!(l.ends_with(" 0100007F"), "adb is published beyond loopback: {l}");
    }
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(), "the loopback port accepts");
    // The guest's own port 5555 isn't a listener in the host's namespace.
    assert!(listeners_on(5555).iter().all(|l| l.ends_with(" 0100007F")) || port == 5555, "{:?}", listeners_on(5555));

    // Stopping the container stops the forward.
    child.kill().expect("kill");
    child.wait().expect("wait");
    drop(net);
    assert!(listeners_on(port).is_empty(), "the forward outlived the container: {:?}", listeners_on(port));
}

/// The `/proc/<pid>/stat` state letter (`S`, `R`, `Z`...), `None` when the process is gone.
fn proc_state(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next().map(String::from)))
}

/// `root` and everything below it, by host pid.
fn family(root: u32) -> Vec<u32> {
    let mut parent_of = vec![];
    for e in std::fs::read_dir("/proc").expect("/proc").flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let ppid = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok())));
        if let Some(ppid) = ppid {
            parent_of.push((pid, ppid));
        }
    }
    let mut all = vec![root];
    let mut i = 0;
    while i < all.len() {
        let at = all[i];
        all.extend(parent_of.iter().filter(|(_, pp)| *pp == at).map(|(p, _)| *p));
        i += 1;
    }
    all
}

const LAUNCHER_DIR: &str = "SIMEE_TEST_LAUNCHER_DIR";

/// Not a test by itself: the launcher process for [`real_host_a_container_dies_with_its_launcher_through_the_whole_chain`],
/// which runs this test binary again with `LAUNCHER_DIR` set. It starts a container the way
/// `launch::start` does (the helper, `unshare`, the id maps), prints its pid and waits.
#[test]
fn real_host_launcher_child() {
    let Some(dir) = std::env::var_os(LAUNCHER_DIR) else { return };
    let tmp = PathBuf::from(dir);
    let (p, _, net) = plan_in(&tmp, tmp.join("rootfs"));
    let p = if probe(&FsView).get(simee_emu_container::probe::Feature::Binder).is_some_and(|c| c.ok()) { p } else { without_binder(&p) };
    let mut p = p;
    if !net {
        p.steps.retain(|s| !matches!(s, Step::Run(_)));
    }
    p.prepare_host().expect("state dir");
    let sh = std::fs::canonicalize("/bin/sh").expect("sh").display().to_string();
    // The container's `/init` environment has a minimal PATH: the stand-in names `sleep` in full.
    let sleep = std::fs::canonicalize("/bin/sleep").expect("sleep").display().to_string();
    let script = p.script_with(&[sh, "-c".into(), format!("echo started; exec {sleep} 600")]);
    let (child, _net) = if net {
        let (c, n) = launch::spawn_container(&p, &script, run_dir(&p), Stdio::null(), Stdio::inherit()).expect("spawn");
        (c, Some(n))
    } else {
        (launch::spawn_isolated(&p.idmap, &script, Some(&p.helper), Stdio::null(), Stdio::inherit()).expect("spawn"), None)
    };
    println!("LAUNCHED {}", child.id());
    // The thread that started the container must outlive it (the parent-death signal follows it).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

/// The real chain (the helper's `die-with-parent`, `unshare`, and for an unprivileged user
/// `become-root` and a second `unshare` for the pid namespace): killing the launcher takes every
/// process of the container with it. The credential change in `become-root` clears the
/// parent-death signal, and it has to be set again.
#[test]
fn real_host_a_container_dies_with_its_launcher_through_the_whole_chain() {
    if !enabled() {
        return;
    }
    let tmp = tmp("chain");
    shell_rootfs(&tmp.join("rootfs"));
    let exe = std::env::current_exe().expect("test exe");
    /// Kills the launcher however the test ends, so a failed assertion leaves nothing running.
    struct Launcher(std::process::Child);
    impl Drop for Launcher {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut launcher = Launcher(
        Command::new(exe)
            .args(["--exact", "real_host_launcher_child", "--nocapture", "--test-threads=1"])
            .env(LAUNCHER_DIR, &tmp)
            .env("SIMEE_E2E_CONTAINER", "1")
            .stdout(Stdio::piped())
            .spawn()
            .expect("the launcher"),
    );
    let mut out = std::io::BufReader::new(launcher.0.stdout.take().expect("stdout"));
    let unshare_pid = loop {
        let mut line = String::new();
        let n = std::io::BufRead::read_line(&mut out, &mut line).expect("launcher output");
        assert!(n > 0, "the launcher exited before it started the container");
        // libtest's own "test name ... " precedes the marker on the same line.
        if let Some((_, pid)) = line.trim().split_once("LAUNCHED ") {
            break pid.parse::<u32>().expect("a pid");
        }
    };
    // Wait for the container's `/init` stand-in (`sleep`): setup is done once it runs.
    let comm = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim().to_string();
    let mut tree = vec![];
    for _ in 0..1000 {
        tree = family(unshare_pid);
        if tree.iter().any(|p| comm(*p) == "sleep") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        tree.iter().any(|p| comm(*p) == "sleep"),
        "the container never reached its init: {:?}",
        tree.iter().map(|p| (*p, comm(*p))).collect::<Vec<_>>()
    );
    eprintln!("container processes: {:?}", tree.iter().map(|p| (*p, comm(*p))).collect::<Vec<_>>());

    launcher.0.kill().expect("kill the launcher");
    launcher.0.wait().expect("reap the launcher");
    // Each is gone, or a zombie waiting to be reaped: poll for the kernel's answer.
    let mut left = tree.clone();
    for _ in 0..5000 {
        left.retain(|p| !matches!(proc_state(*p).as_deref(), None | Some("Z")));
        if left.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        left.is_empty(),
        "the container outlived its launcher: {:?}",
        left.iter().map(|p| (*p, comm(*p), proc_state(*p))).collect::<Vec<_>>()
    );
}

#[test]
fn real_host_a_container_dies_with_its_launcher() {
    if !enabled() {
        return;
    }
    // `sh` stands in for the launcher: it starts the helper (parent = that sh), the helper asks
    // for SIGKILL when its parent dies and becomes `sleep`. Killing the sh must kill the sleep.
    let script = format!("{} die-with-parent --parent $$ -- sleep 600 & echo $!; wait", helper().program.display());
    let mut launcher = Command::new("sh").args(["-c", &script]).stdout(Stdio::piped()).spawn().expect("sh");
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(launcher.stdout.as_mut().expect("stdout")), &mut line).expect("pid line");
    let pid: u32 = line.trim().parse().expect("a pid");
    let state = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next().map(String::from)))
    };
    assert!(matches!(state(pid).as_deref(), Some("S" | "R")), "the sleep is running: {:?}", state(pid));
    launcher.kill().expect("kill the launcher");
    launcher.wait().expect("reap");
    // Gone, or a zombie waiting for its adopter to reap it. Poll for the kernel's answer.
    let mut gone = false;
    for _ in 0..5000 {
        if matches!(state(pid).as_deref(), None | Some("Z")) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(gone, "the sleep outlived its launcher: {:?}", state(pid));
}

#[test]
fn real_host_the_helper_refuses_when_its_launcher_is_already_gone() {
    if !enabled() {
        return;
    }
    // A wrong parent pid: the helper must not exec the command.
    let out = Command::new(helper().program).args(["die-with-parent", "--parent", "1", "--", "echo", "ran"]).output().expect("helper");
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("ran"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already gone"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn real_host_the_probe_agrees_with_the_kernel_about_binder() {
    if !enabled() {
        return;
    }
    let view = FsView;
    let registered =
        std::fs::read_to_string("/proc/filesystems").is_ok_and(|f| f.lines().any(|l| l.split_whitespace().last() == Some("binder")));
    let ok = probe(&view).get(simee_emu_container::probe::Feature::Binder).is_some_and(|c| c.ok());
    if !registered {
        assert!(!ok);
    } else if let Some(devices) = view.binderfs_devices() {
        assert_eq!(ok, ["binder", "hwbinder", "vndbinder"].iter().all(|d| devices.iter().any(|x| x == d)), "{devices:?}");
    }
}
