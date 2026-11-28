//! Software mode (#55): booting an x86_64 AVD on a Linux host. The guest's CPU runs in QEMU on
//! stock's x86 board, on the host's KVM when `/dev/kvm` is usable and in QEMU's multi-threaded TCG
//! otherwise (`-accel`, as stock's); emu serves everything else (`emu_vmm::soft`), and the console
//! port, `display.sock` and adb work as on a Mac.
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use emu_vmm::soft::Accel;

use crate::{args, console, display_server, instance};

/// What to tell someone without a usable QEMU.
const NEED_QEMU: &str =
    "software mode runs the guest's CPU in QEMU 11.1 or newer: install it (qemu-system-x86) or point EMU_QEMU at a qemu-system-x86_64";

/// `-accel-check` in software mode: usable when a new enough QEMU is there, with what runs the
/// guest, as stock prints its accelerator.
pub fn accel_check() -> (bool, String) {
    let qemu = emu_vmm::soft::find_qemu(std::env::var("EMU_QEMU").ok().as_deref(), &std::env::var("PATH").unwrap_or_default())
        .ok_or_else(|| NEED_QEMU.to_string())
        .and_then(|q| emu_vmm::soft::check_qemu(&q).map(|v| (q, v)).map_err(|e| e.to_string()));
    let kvm = match &qemu {
        Ok((q, _)) => kvm(q),
        Err(_) => kvm_device(),
    };
    accel_line(qemu, kvm)
}

/// Whether this process can run guests on KVM: `/dev/kvm` opens, has what QEMU needs, and makes
/// a VM (which fails with EBUSY while another hypervisor holds VT-x on kernels before 6.12). The
/// error says why not and how to fix it.
fn kvm_device() -> std::result::Result<(), String> {
    emu_kvm::open().and_then(|k| k.create_vm(0)).map(drop).map_err(|e| e.to_string())
}

/// [`kvm_device`], and `qemu` was built with KVM (`-accel help` lists it).
fn kvm(qemu: &std::path::Path) -> std::result::Result<(), String> {
    kvm_device()?;
    let out = std::process::Command::new(qemu).args(["-accel", "help"]).output().map_err(|e| format!("running {}: {e}", qemu.display()))?;
    if lists_kvm(&String::from_utf8_lossy(&out.stdout)) {
        Ok(())
    } else {
        Err(format!("{} was built without KVM (its -accel help doesn't list kvm)", qemu.display()))
    }
}

/// Whether QEMU's `-accel help` output names KVM.
fn lists_kvm(help: &str) -> bool {
    help.lines().skip(1).any(|l| l.trim() == "kvm")
}

/// [`accel_check`]'s verdict from what it found.
fn accel_line(qemu: std::result::Result<(std::path::PathBuf, (u32, u32)), String>, kvm: std::result::Result<(), String>) -> (bool, String) {
    match qemu {
        Ok((path, (major, minor))) => (
            true,
            match kvm {
                Ok(()) => format!("software mode: QEMU {major}.{minor} on KVM at {} with emu's devices", path.display()),
                Err(why) => format!(
                    "software mode: QEMU {major}.{minor} TCG at {} with emu's devices (no hardware acceleration: {why})",
                    path.display()
                ),
            },
        ),
        Err(why) => (false, why),
    }
}

/// What runs the guest for `-accel` (`want`: None is auto) given whether KVM is usable: KVM when
/// it is and isn't turned off, TCG otherwise; `-accel on` without KVM is an error.
fn pick_accel(want: Option<bool>, kvm: std::result::Result<(), String>) -> Result<(Accel, Option<String>)> {
    match (want, kvm) {
        (Some(false), _) => Ok((Accel::Tcg, Some("-accel off".into()))),
        (_, Ok(())) => Ok((Accel::Kvm, None)),
        (None, Err(why)) => Ok((Accel::Tcg, Some(why))),
        (Some(true), Err(why)) => anyhow::bail!("-accel on: {why}"),
    }
}

pub fn boot(
    env: &emu_avd::Env,
    name: &str,
    opts: &args::Opts,
    signals: std::sync::mpsc::Receiver<i32>,
    _launched: std::time::Instant,
) -> Result<ExitCode> {
    let avd = env.load(name)?;
    // Before anything is created or started, as on a Mac: refusing takes milliseconds.
    if !instance::allow_old_images()
        && let Some(why) = instance::unsupported(&avd)
    {
        anyhow::bail!(why);
    }
    let qemu = emu_vmm::soft::find_qemu(std::env::var("EMU_QEMU").ok().as_deref(), &std::env::var("PATH").unwrap_or_default())
        .context(NEED_QEMU)?;
    let (major, minor) = emu_vmm::soft::check_qemu(&qemu)?;
    let (accel, why_tcg) = pick_accel(opts.accel, if opts.accel == Some(false) { Ok(()) } else { kvm(&qemu) })?;
    if opts.verbose {
        let on = match (accel, why_tcg) {
            (Accel::Kvm, _) => "KVM".to_string(),
            (Accel::Tcg, why) => format!("TCG ({})", why.unwrap_or_default()),
        };
        eprintln!("emu: software mode: QEMU {major}.{minor} at {}, guest CPU on {on}", qemu.display());
        if !opts.ignored.is_empty() {
            eprintln!("emu: accepted but not implemented yet: {}", opts.ignored.join(" "));
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::Builder::new().name("stop-watch".into()).spawn(move || {
            // QEMU gets 10 s to quit (`emu_vmm::soft`); don't outlive that by much, and a second
            // signal exits at once.
            let limit = std::time::Duration::from_secs(15);
            let Some((sig, forced)) = crate::on_signal::watch(&signals, limit, || stop.store(true, Ordering::SeqCst)) else { return };
            match forced {
                crate::on_signal::Forced::Second(_) => eprintln!("emu: a second signal: exiting without finishing the shutdown"),
                crate::on_signal::Forced::Overrun => eprintln!("emu: shutdown took over 15 s; exiting"),
            }
            std::process::exit(crate::on_signal::exit_code(sig));
        })?;
    }

    let inst = instance::Instance::prepare(&avd, opts)?;
    opts.camera_links(|k| avd.get(k).map(str::to_string))
        .into_iter()
        .for_each(|(f, s)| emu_vmm::pipe::camera::Cameras::global().link(f, s));
    let adb_port = inst.console_port + 1;
    let _ = std::thread::Builder::new().name("adb-announce".into()).spawn(move || {
        loop {
            std::thread::sleep(crate::ADB_REANNOUNCE);
            emu_vmm::pipe::adb::AdbService::announce_port(adb_port);
        }
    });
    console::serve(
        inst.console_port,
        console::ConsoleInfo {
            avd_name: avd.name.clone(),
            stop: stop.clone(),
            display: inst.display.clone(),
            inputs: inst.inputs.clone(),
            sensors: inst.sensors.clone(),
            clipboard: inst.clipboard.clone(),
            fingerprint: inst.fingerprint.clone(),
            gps: inst.gps.clone(),
            battery: inst.battery.clone(),
            telephony: inst.telephony.clone(),
            xr: inst.xr.clone(),
            car: inst.car.clone(),
            network: inst.network.clone(),
            // Foldable postures aren't wired in software mode yet.
            fold: None,
            rotation_unknown: instance::rotation_unknown(&avd),
        },
    )?;
    let _advert = console::Advertisement::write(&avd.name, &avd.dir, inst.console_port);
    let _display = inst.display.clone().and_then(|d| display_server::start(&avd.dir, d, inst.inputs.clone(), stop.clone()));
    // On stdout: launchers watch for the first line and the `emulator-<port>` serial.
    println!("emu: {} on emulator-{} (adb port {adb_port})", avd.name, inst.console_port);
    let cfg = inst.soft_config(opts, qemu, accel)?;
    match emu_vmm::soft::run(cfg, stop)? {
        // 130 or 143 when a signal stopped it.
        emu_vmm::soft::Exit::PowerOff | emu_vmm::soft::Exit::Stopped => Ok(crate::on_signal::stopped()),
        emu_vmm::soft::Exit::Failed(why) => {
            eprintln!("emu: {why}");
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accel_check_says_what_runs_the_guest_or_what_is_missing() {
        let found = Ok((std::path::PathBuf::from("/q/qemu-system-x86_64"), (11, 1)));
        let (ok, line) = accel_line(found.clone(), Err("/dev/kvm doesn't exist".into()));
        assert!(ok);
        assert_eq!(
            line,
            "software mode: QEMU 11.1 TCG at /q/qemu-system-x86_64 with emu's devices (no hardware acceleration: /dev/kvm doesn't exist)"
        );
        assert_eq!(accel_line(found, Ok(())), (true, "software mode: QEMU 11.1 on KVM at /q/qemu-system-x86_64 with emu's devices".into()));
        let (ok, line) = accel_line(Err(NEED_QEMU.to_string()), Ok(()));
        assert!(!ok && line.contains("QEMU 11.1 or newer"), "{line}");
    }

    #[test]
    fn qemus_accelerator_list_says_whether_it_has_kvm() {
        assert!(lists_kvm("Accelerators supported in QEMU binary:\ntcg\nkvm\n"));
        assert!(!lists_kvm("Accelerators supported in QEMU binary:\ntcg\n"));
        assert!(!lists_kvm("kvm\n"), "the heading line isn't an accelerator");
    }

    #[test]
    fn kvm_runs_the_guest_unless_it_is_turned_off_or_unusable() {
        let denied = || Err::<(), _>("/dev/kvm exists but this user can't open it".to_string());
        assert_eq!(pick_accel(None, Ok(())).expect("auto").0, Accel::Kvm);
        assert_eq!(pick_accel(Some(true), Ok(())).expect("on").0, Accel::Kvm);
        assert_eq!(pick_accel(Some(false), Ok(())).expect("off"), (Accel::Tcg, Some("-accel off".into())));
        let (accel, why) = pick_accel(None, denied()).expect("auto falls back");
        assert_eq!((accel, why.as_deref()), (Accel::Tcg, Some("/dev/kvm exists but this user can't open it")));
        let err = pick_accel(Some(true), denied()).expect_err("on needs KVM").to_string();
        assert!(err.starts_with("-accel on: /dev/kvm exists"), "{err}");
    }
}
