//! The two things the container's launcher needs a process for that a shell script can't do, and
//! that safe Rust can through `rustix`'s wrappers (so this crate still has no `unsafe`):
//!
//! - `die-with-parent --parent <pid> -- <command…>`: asks the kernel to SIGKILL this process when
//!   its parent dies (`PR_SET_PDEATHSIG`), then execs the command, which keeps the setting. Run
//!   first in the chain `helper → unshare → sh`, it makes `unshare` die with the launcher, and
//!   `unshare --kill-child` takes the container's pid namespace down with it: a crashed `emu` leaves
//!   no orphaned Android holding the host's adb port. The kernel's "parent" is the *thread* that
//!   spawned the child, so the container must be started from a thread that outlives it.
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
        [cmd, root, sep, rest @ ..] if cmd == "pivot" && sep == "--" => {
            if !root.starts_with('/') {
                return Err(format!("pivot root {root} must be an absolute path"));
            }
            Ok(Request::Pivot { root: root.clone(), init: split(rest)? })
        }
        _ => Err("usage: die-with-parent --parent <pid> -- <command…> | pivot <root> -- <init…>".into()),
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
            "frobnicate",
        ] {
            assert!(parse(&a(bad)).is_err(), "{bad:?}");
        }
    }
}
