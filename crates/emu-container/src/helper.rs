//! The three things the container's launcher needs a process for that a shell script can't do, and
//! that safe Rust can through `rustix`'s wrappers (so this crate still has no `unsafe`):
//!
//! - `die-with-parent --parent <pid> -- <command…>`: asks the kernel to SIGKILL this process when
//!   its parent dies (`PR_SET_PDEATHSIG`), then execs the command, which keeps the setting. Run
//!   first in the chain `helper → unshare → sh`, it makes `unshare` die with the launcher, and
//!   `unshare --kill-child` takes the container's pid namespace down with it: a crashed `emu` leaves
//!   no orphaned Android holding the host's adb port. The kernel's "parent" is the *thread* that
//!   spawned the child, so the container must be started from a thread that outlives it.
//! - `become-root -- <command…>`: for an unprivileged user's container, whose root is a subordinate
//!   id and never the user's own uid. `unshare --user --keep-caps` leaves the process with its
//!   capabilities (as ambient ones) but an unmapped uid, since the user's uid isn't in the map.
//!   This waits until `newuidmap` and `newgidmap` have written the maps, clears the supplementary
//!   groups, becomes uid and gid 0 (the subordinate range's first id) and execs the command, which
//!   as root of the namespace has its capabilities in full.
//! - `pivot <root> -- <init…>`: `pivot_root` into `root` and lazily unmount the old root, so no
//!   path to the host's filesystem is left in the container's mount namespace (a `chroot` leaves
//!   one, and root inside it can walk out). Then exec `/init` with a clean environment. It runs
//!   *before* the old root is gone, so a dynamically linked helper still loads; after the pivot it
//!   needs nothing from it.
//!
//! The helper is whichever executable the caller names ([`crate::launch::Helper`]): `emu` itself
//! under a hidden subcommand, or this crate's `simee-container-helper` binary.

use std::process::ExitCode;

/// What a helper invocation asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    DieWithParent { parent: u32, command: Vec<String> },
    BecomeRoot { command: Vec<String> },
    Pivot { root: String, init: Vec<String> },
}

/// The environment `/init` starts with (Android's init sets up the rest).
pub const INIT_PATH: &str = "/sbin:/system/sbin:/system/bin:/system/xbin:/vendor/bin:/vendor/xbin";

/// Parses the arguments after the helper's own name.
pub fn parse(args: &[String]) -> Result<Request, String> {
    let split = |rest: &[String]| -> Result<Vec<String>, String> {
        let command = rest.to_vec();
        if command.is_empty() { Err("no command after `--`".into()) } else { Ok(command) }
    };
    match args {
        [cmd, flag, pid, sep, rest @ ..] if cmd == "die-with-parent" && flag == "--parent" && sep == "--" => {
            Ok(Request::DieWithParent { parent: pid.parse().map_err(|_| format!("--parent {pid}: not a pid"))?, command: split(rest)? })
        }
        [cmd, sep, rest @ ..] if cmd == "become-root" && sep == "--" => Ok(Request::BecomeRoot { command: split(rest)? }),
        [cmd, root, sep, rest @ ..] if cmd == "pivot" && sep == "--" => {
            if !root.starts_with('/') {
                return Err(format!("pivot root {root} must be an absolute path"));
            }
            Ok(Request::Pivot { root: root.clone(), init: split(rest)? })
        }
        _ => Err("usage: die-with-parent --parent <pid> -- <command…> | become-root -- <command…> | pivot <root> -- <init…>".into()),
    }
}

/// Runs the helper: returns only on failure (success replaces the process).
pub fn main(args: &[String]) -> ExitCode {
    let result = parse(args).and_then(|r| run(&r));
    // `run` returns only when something went wrong.
    eprintln!("emu: container helper: {}", result.err().unwrap_or_default());
    ExitCode::from(1)
}

#[cfg(target_os = "linux")]
fn run(req: &Request) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    match req {
        Request::DieWithParent { parent, command } => {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(|e| format!("PR_SET_PDEATHSIG: {e}"))?;
            // The parent may have died before the signal was set: then nothing will ever send it.
            if rustix::process::Pid::as_raw(rustix::process::getppid()) as u32 != *parent {
                return Err(format!("the launcher (pid {parent}) is already gone"));
            }
            let err = Command::new(&command[0]).args(&command[1..]).exec();
            Err(format!("running {}: {err}", command[0]))
        }
        Request::BecomeRoot { command } => {
            use rustix::process::{Gid, Uid};
            use rustix::thread::{set_thread_groups, set_thread_res_gid, set_thread_res_uid};
            wait_for_maps(MAP_DEADLINE)?;
            let at = |what: &'static str| move |e: rustix::io::Errno| format!("{what}: {e}");
            // Groups, then gid, then uid: after the uid change the process may not be allowed the rest.
            set_thread_groups(&[]).map_err(at("clearing the supplementary groups"))?;
            set_thread_res_gid(Gid::ROOT, Gid::ROOT, Gid::ROOT).map_err(at("becoming gid 0"))?;
            set_thread_res_uid(Uid::ROOT, Uid::ROOT, Uid::ROOT).map_err(at("becoming uid 0"))?;
            let err = Command::new(&command[0]).args(&command[1..]).exec();
            Err(format!("running {}: {err}", command[0]))
        }
        Request::Pivot { root, init } => {
            use rustix::mount::{MountPropagationFlags, UnmountFlags, mount_change, unmount};
            let at = |what: &'static str| move |e: rustix::io::Errno| format!("{what}: {e}");
            rustix::process::chdir(root.as_str()).map_err(at("entering the new root"))?;
            // Nothing done below may propagate to (or from) the host's mounts.
            mount_change(".", MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC)
                .map_err(at("making mounts private to the container"))?;
            // Stack the old root on the new one, then detach it: no path back to the host is left.
            rustix::process::pivot_root(".", ".").map_err(at("pivot_root"))?;
            unmount(".", UnmountFlags::DETACH).map_err(at("detaching the old root"))?;
            rustix::process::chdir("/").map_err(at("entering /"))?;
            let err = Command::new(&init[0]).args(&init[1..]).env_clear().env("PATH", INIT_PATH).exec();
            Err(format!("running {}: {err}", init[0]))
        }
    }
}

/// How long `become-root` waits for the parent to write the id maps (milliseconds in practice).
#[cfg(target_os = "linux")]
const MAP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Waits until this process's user namespace has a uid and a gid map, which the parent writes
/// after `unshare` made the namespace. Polls every millisecond.
#[cfg(target_os = "linux")]
fn wait_for_maps(deadline: std::time::Duration) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        let mapped = |f: &str| std::fs::read_to_string(f).is_ok_and(|s| !s.trim().is_empty());
        if mapped("/proc/self/uid_map") && mapped("/proc/self/gid_map") {
            return Ok(());
        }
        if start.elapsed() >= deadline {
            return Err(format!("the launcher wrote no id maps within {deadline:?}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_: &Request) -> Result<(), String> {
    Err("containers need Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_both_requests() {
        assert_eq!(
            parse(&a("die-with-parent --parent 42 -- unshare --user -- sh")),
            Ok(Request::DieWithParent { parent: 42, command: a("unshare --user -- sh") })
        );
        assert_eq!(parse(&a("become-root -- sh -c true")), Ok(Request::BecomeRoot { command: a("sh -c true") }));
        assert_eq!(parse(&a("pivot /r -- /init qemu=1")), Ok(Request::Pivot { root: "/r".into(), init: a("/init qemu=1") }));
    }

    #[test]
    fn refuses_what_it_cant_run() {
        for bad in [
            "",
            "pivot",
            "pivot r -- /init",
            "pivot /r --",
            "pivot /r /init",
            "die-with-parent --parent x -- sh",
            "die-with-parent --parent 1 --",
            "become-root",
            "become-root --",
            "become-root sh",
            "frobnicate",
        ] {
            assert!(parse(&a(bad)).is_err(), "{bad:?}");
        }
    }
}
