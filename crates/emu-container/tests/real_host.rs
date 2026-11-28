//! The launcher against the real kernel. Each test needs `unshare`, `sh`, `ldd` and a kernel that
//! lets this user mount an overlay inside a user namespace, and returns at once unless
//! `SIMEE_E2E_CONTAINER=1`. Run as root and as an unprivileged user with a `/etc/subuid` range.
//!
//! On a host without binder the mount sequence stops at binderfs, and that message is the
//! assertion there; the rest runs with the binder steps filtered out.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use simee_emu_container::launch::{self, Display, GpuMode, Helper, IdMap, Plan, Spec, Step};
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

/// A root filesystem holding only the host's shell and the libraries it links, so a script can
/// run after the pivot.
fn shell_rootfs(rootfs: &Path) -> String {
    let sh = std::fs::canonicalize("/bin/sh").expect("sh");
    let ldd = Command::new("ldd").arg(&sh).output().expect("ldd");
    let mut files = vec![sh.clone()];
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
    sh.display().to_string()
}

fn plan_in(dir: &Path, rootfs: PathBuf) -> (Plan, IdMap) {
    let idmap = IdMap::for_host(&FsView, rustix::process::getegid().as_raw()).expect("an id mapping for this user");
    let spec = Spec {
        dir: dir.join("device"),
        rootfs,
        entrypoint: ["/init", "qemu=1", "androidboot.hardware=redroid"].map(String::from).to_vec(),
        display: Display { width: 720, height: 1280, dpi: 320, fps: None },
        gpu: GpuMode::Guest,
        props: vec![],
    };
    (launch::plan(&spec, &FsView, idmap.clone(), helper()).expect("plan"), idmap)
}

/// What a script run in the container's namespaces prints.
fn run(p: &Plan, argv: &[String]) -> (bool, String, String) {
    p.prepare_host().expect("state dir");
    let out = launch::spawn_isolated(&p.idmap, &p.script_with(argv), Some(&p.helper), Stdio::piped(), Stdio::piped())
        .expect("spawn")
        .wait_with_output()
        .expect("wait");
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
"#;

#[test]
fn real_host_mounts_pivots_and_leaves_no_path_to_the_host() {
    if !enabled() {
        return;
    }
    let tmp = tmp("mounts");
    let rootfs = tmp.join("rootfs");
    let sh = shell_rootfs(&rootfs);
    let (p, idmap) = plan_in(&tmp, rootfs);
    // The state directory belongs to container root (root-run), whose range the maps give.
    let argv = vec![sh.clone(), "-c".into(), INSPECT.into()];
    let binder = probe(&FsView).get(simee_emu_container::probe::Feature::Binder).is_some_and(|c| c.ok());
    let (ok, stdout, stderr) = run(&p, &argv);
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
        let mut rest = p.clone();
        rest.steps.retain(|s| match s {
            Step::Mount(m) => m.fstype != Some("binder"),
            Step::Require { .. } => false,
            Step::Symlink { link, .. } => !link.to_string_lossy().contains("binder"),
            _ => true,
        });
        let (ok, stdout, stderr) = run(&rest, &argv);
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
        IdMap::Rootless { uid, .. } => assert_eq!(first[1], uid.to_string()),
    }
    assert_ne!(first[1], "0", "the container's root is not the host's root: {map:?}");
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
