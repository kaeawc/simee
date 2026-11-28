//! `emu`: boots an Android Virtual Device with the simee VMM.
//!
//! Accepts the SDK `emulator` command line (single-dash flags, `-avd NAME` or `@NAME`) so
//! existing tooling can launch it unchanged; flags that don't apply yet are accepted and
//! reported once with `-verbose`.

mod args;
// The VMM, the renderer and everything that drives them exist only on Apple silicon Macs (the
// Hypervisor.framework); elsewhere `emu` still lists, creates and provisions AVDs.
mod console;
// Builds and tests everywhere; only an Apple silicon boot serves it.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
mod display_server;
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
mod fold;
mod golden;
#[cfg(windows)]
mod host_windows;
mod instance;
mod on_signal;
mod provision;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod software;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod vkprobe;

#[cfg(windows)]
use host_windows::raise_nofile;
use std::process::ExitCode;

use anyhow::{Context, Result};

/// macOS's `OPEN_MAX`: the most open files `setrlimit` grants a process there whatever its hard
/// limit says, and room for the NAT's flow caps and the rest of emu.
#[cfg_attr(not(unix), allow(dead_code))]
const NOFILE_WANT: u64 = 10240;

/// Raises the soft open-file limit (256 by default on macOS) toward the hard one, up to
/// [`NOFILE_WANT`]: each of the guest's network flows is a host socket of this process, and
/// running out also fails the console's and the adb bridge's `accept`. Never lowers it. Returns
/// the soft limit in force.
#[cfg(unix)]
fn raise_nofile() -> u64 {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let cur = getrlimit(Resource::Nofile);
    let soft = cur.current.unwrap_or(u64::MAX);
    let want = cur.maximum.unwrap_or(u64::MAX).min(NOFILE_WANT);
    if soft >= want {
        return soft;
    }
    match setrlimit(Resource::Nofile, Rlimit { current: Some(want), maximum: cur.maximum }) {
        Ok(()) => want,
        Err(e) => {
            eprintln!("emu: warning: the open-file limit stays {soft} ({e}): the guest gets fewer network flows");
            soft
        }
    }
}

/// How often a running instance tells the adb server about itself again.
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
const ADB_REANNOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a stop may take before the stop watcher exits the process anyway.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
const SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The teardown step once the machine is down (`emu_vmm::teardown_step`).
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
const EXITING: &str = "exiting after the machine stopped";

/// What the stop watcher prints when a stop overran [`SHUTDOWN_LIMIT`]: the teardown step it was
/// waiting for (as `emu_vmm::teardown_status` gives it) and for how long.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn overrun_line(step: Option<(String, std::time::Duration)>) -> String {
    let limit = SHUTDOWN_LIMIT.as_secs();
    match step {
        Some((what, t)) => format!("shutdown took over {limit} s ({what}, for {:.1} s); exiting", t.as_secs_f64()),
        None => format!("shutdown took over {limit} s (the machine had not started tearing down); exiting"),
    }
}

/// `-verbose`'s exit-count lines, one per vCPU, without the zero counters.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn exit_lines(exits: &[Vec<(&'static str, u64)>]) -> Vec<String> {
    exits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let counts: Vec<String> = e.iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{k}={n}")).collect();
            format!("  vcpu{i}: {}", counts.join(" "))
        })
        .collect()
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("emu: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `-create-avd`: writes the AVD shaped like its image's form factor (a round watch, a TV, an XR
/// headset, ...), with the requested hardware on top, warning if its image is missing. Then its
/// image's golden user data (`golden.rs`) unless `-no-golden`.
fn create_avd(env: &emu_avd::Env, name: &str, sysdir: &str, opts: &args::Opts) -> Result<()> {
    let mut spec = emu_avd::NewAvd::for_image(name, sysdir);
    // The profile first, so an explicit -lcd, -cores or -memory still wins over it.
    if let Some(profile) = &opts.profile {
        spec = spec.with_profile(profile)?;
    }
    if let Some(lcd) = opts.lcd {
        spec.lcd = lcd;
    }
    if let Some(c) = opts.cores {
        spec.cores = c;
    }
    if let Some(m) = opts.memory_mb {
        spec.ram_mb = m;
    }
    let dir = env.create_avd(&spec)?;
    // Fail now, not at first boot, if the image isn't installed.
    if let Err(e) = env.load(name) {
        eprintln!("emu: warning: {e} (install it with emu -provision <api>)");
    }
    println!("created {name} at {}", dir.display());
    // Started from its image's settled user data (booting the image once if it has none): after
    // the AVD is reported created, which it is whatever happens next.
    golden::start_from_golden(env, name, opts);
    Ok(())
}

fn real_main() -> Result<ExitCode> {
    let launched = std::time::Instant::now();
    // First, before any thread exists.
    let signals = on_signal::shutdown_signals();
    // Before the NAT sizes its flow caps from it.
    raise_nofile();
    // Internal: `emu -probe-vulkan <lib64>`, `emu -probe-kosmickrisp <lib64>` and
    // `emu -warm-renderer <lib64>` (see vkprobe.rs).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let raw: Vec<String> = std::env::args().collect();
        let child: Option<fn(&std::path::Path) -> bool> = match raw.get(1).map(String::as_str) {
            Some("-probe-vulkan") => Some(vkprobe::probe),
            Some("-probe-kosmickrisp") => Some(vkprobe::probe_kosmickrisp),
            Some("-warm-renderer") => Some(vkprobe::warm_child),
            _ => None,
        };
        if let Some(run) = child {
            let ok = raw.get(2).is_some_and(|p| run(std::path::Path::new(p)));
            // A clean "no" has a status of its own: a crash, a panic or a signal isn't a verdict.
            return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::from(vkprobe::PROBE_FAILED) });
        }
    }
    let opts = args::parse(std::env::args().skip(1))?;
    let env = emu_avd::Env::from_env();

    if opts.help {
        print!("{}", args::USAGE);
        return Ok(ExitCode::SUCCESS);
    }
    if opts.version {
        println!("simee {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    if opts.list_avds {
        for name in env.list_avds() {
            println!("{name}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if opts.list_images {
        provision::list_images(&env, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(api) = &opts.provision {
        provision::provision(&env, api, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if opts.provision_renderer {
        provision::provision_renderer(&env, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if opts.provision_dhu {
        provision::provision_dhu(&env, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(name) = &opts.create_avd {
        let sysdir = opts.sysdir.clone().context("-create-avd needs -sysdir system-images/<target>/<tag>/<abi>")?;
        create_avd(&env, name, &sysdir, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(name) = &opts.clone_avd {
        let golden = opts.from.as_deref().context("-clone-avd needs -from GOLDEN")?;
        let dir = env.clone_avd(golden, name)?;
        println!("cloned {golden} as {name} at {}", dir.display());
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(name) = &opts.delete_avd {
        env.delete_avd(name)?;
        println!("deleted {name}");
        return Ok(ExitCode::SUCCESS);
    }
    if opts.accel_check {
        let (ok, what) = accel_check();
        println!("accel:\n{}\n{what}\naccel", if ok { 0 } else { 1 });
        return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
    }
    let name = opts.avd.clone().context("no AVD given (use -avd NAME, or -list-avds)")?;
    if opts.dry_run {
        return dry_run(&env, &name, &opts);
    }
    boot(&env, &name, &opts, signals, launched)
}

/// `-accel-check`: whether this host can boot a guest, and what runs it or what's missing (stock
/// prints the same `accel:` block).
fn accel_check() -> (bool, String) {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return match android_support(host_macos()) {
        Ok(off) => {
            let mut what = String::from("Hypervisor.framework (arm64) is usable");
            for why in off {
                what.push('\n');
                what.push_str(&why);
            }
            (true, what)
        }
        Err(why) => (false, why),
    };
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return software::accel_check();
    #[cfg(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))))]
    (false, "simee needs Hypervisor.framework on an Apple silicon Mac, or software mode on x86_64 Linux".into())
}

/// This host's macOS release (`SystemVersion.plist`), `None` off macOS.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn host_macos() -> Option<sim_core::support::Version> {
    let text = std::fs::read_to_string(sim_core::support::SYSTEM_VERSION_PLIST).ok()?;
    sim_core::support::Version::from_system_version(&text).map(|(v, _)| v)
}

/// The support matrix's Android rows (`sim_core::support`, `simee doctor`) on macOS `macos`: an
/// error when no guest can boot here (below the floor), else why each optional feature is off.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn android_support(macos: Option<sim_core::support::Version>) -> std::result::Result<Vec<String>, String> {
    use sim_core::support::{FLOORS, check};
    check("android", macos)?;
    Ok(FLOORS.iter().filter(|f| f.requires == Some("android")).filter_map(|f| check(f.id, macos).err()).collect())
}

/// `-avd NAME -dry-run`: prints the boot plan without booting (on any host), and fails if the
/// image is missing files the boot reads.
fn dry_run(env: &emu_avd::Env, name: &str, opts: &args::Opts) -> Result<ExitCode> {
    let avd = env.load(name)?;
    let renderer = if cfg!(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))) {
        instance::renderer(&avd, opts)
    } else {
        None
    };
    let plan = instance::BootPlan::new(&avd, opts, renderer);
    print!("{plan}");
    let missing = plan.missing();
    for path in &missing {
        eprintln!("emu: missing {}", path.display());
    }
    let unsupported = instance::unsupported(&avd).filter(|_| !instance::allow_old_images());
    if let Some(why) = &unsupported {
        eprintln!("emu: {why}");
    }
    Ok(if missing.is_empty() && unsupported.is_none() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))))]
fn boot(_: &emu_avd::Env, name: &str, _: &args::Opts, _: std::sync::mpsc::Receiver<i32>, _: std::time::Instant) -> Result<ExitCode> {
    anyhow::bail!("cannot boot {name}: simee needs Hypervisor.framework on an Apple silicon Mac, or an x86_64 Linux host for software mode")
}

/// Once the guest has booted, a foldable tells it its posture (stock sends it after boot).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn announce_fold_at_boot(cfg: &mut emu_vmm::MachineConfig, fold: Option<std::sync::Arc<fold::Foldable>>) {
    let Some(fold) = fold else { return };
    let before = cfg.on_guest_event.take();
    cfg.on_guest_event = Some(std::sync::Arc::new(move |event: &str| {
        if let Some(f) = &before {
            f(event);
        }
        if event == "bootcomplete" {
            fold.announce();
        }
    }));
}

/// Software mode (#55): x86_64 AVDs on QEMU's TCG with emu's devices.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn boot(
    env: &emu_avd::Env,
    name: &str,
    opts: &args::Opts,
    signals: std::sync::mpsc::Receiver<i32>,
    launched: std::time::Instant,
) -> Result<ExitCode> {
    software::boot(env, name, opts, signals, launched)
}

/// Boots the AVD and runs it until the guest powers off or a signal arrives.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn boot(
    env: &emu_avd::Env,
    name: &str,
    opts: &args::Opts,
    signals: std::sync::mpsc::Receiver<i32>,
    launched: std::time::Instant,
) -> Result<ExitCode> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Before anything is created or started: refusing takes milliseconds, and the error is the
    // last line front ends show (`emu exited … before booting: <line>`).
    for off in android_support(host_macos()).map_err(anyhow::Error::msg)? {
        eprintln!("emu: {off}");
    }
    if !instance::allow_old_images()
        && let Some(why) = instance::unsupported(&env.load(name)?)
    {
        anyhow::bail!(why);
    }
    if opts.verbose && !opts.ignored.is_empty() {
        eprintln!("emu: accepted but not implemented yet: {}", opts.ignored.join(" "));
    }

    let stop = Arc::new(AtomicBool::new(false));
    // A foldable, once there is one: it resets a size it forced before the guest stops.
    let foldable: Arc<std::sync::OnceLock<Arc<fold::Foldable>>> = Arc::default();
    {
        let stop = stop.clone();
        let foldable = foldable.clone();
        // Without it a SIGINT/SIGTERM would never stop the machine: fail the launch instead.
        let verbose = opts.verbose;
        std::thread::Builder::new().name("stop-watch".into()).spawn(move || {
            // As the console's `kill` stops it. Device threads blocked in native code (e.g. a
            // wedged renderer) can stall an orderly teardown; a second signal, or the limit,
            // doesn't let that keep the process alive.
            let stopping = || {
                if let Some(f) = foldable.get() {
                    f.release();
                }
                emu_vmm::request_stop(&stop);
            };
            let Some((sig, forced)) = on_signal::watch(&signals, SHUTDOWN_LIMIT, stopping) else { return };
            if forced != on_signal::Forced::Overrun {
                eprintln!("emu: a second signal: exiting without finishing the shutdown");
                std::process::exit(on_signal::exit_code(sig));
            }
            let step = emu_vmm::teardown_status();
            // The orderly path prints the counts once the machine is down; it didn't get there.
            let counts = verbose && step.as_ref().is_none_or(|(s, _)| s != EXITING);
            eprintln!("emu: {}", overrun_line(step));
            if let Some(exits) = emu_vmm::exit_counts().filter(|_| counts) {
                for line in exit_lines(&exits) {
                    eprintln!("emu: {line}");
                }
            }
            std::process::exit(on_signal::exit_code(sig));
        })?;
    }

    let avd = env.load(name)?;
    let inst = instance::Instance::prepare(&avd, opts)?;
    opts.camera_links(|k| avd.get(k).map(str::to_string))
        .into_iter()
        .for_each(|(f, s)| emu_vmm::pipe::camera::Cameras::global().link(f, s));
    {
        // An adb server finds emulators by scanning adb ports 5555..5585 (16 instances, unless
        // started with ADB_LOCAL_TRANSPORT_MAX_PORT), or by being told (`host:emulator:<port>`,
        // sent at boot). A server started later (adb kill-server, a reboot of adb) never hears
        // of an instance above 5585, so it's told again every few seconds: for a device it
        // already has, the server ignores it (one loopback connect and write).
        let adb_port = inst.console_port + 1;
        let _ = std::thread::Builder::new().name("adb-announce".into()).spawn(move || {
            emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
            loop {
                std::thread::sleep(ADB_REANNOUNCE);
                emu_vmm::pipe::adb::AdbService::announce_port(adb_port);
            }
        });
    }
    let fold = fold::for_avd(&avd, inst.sensors.clone(), inst.display.clone(), inst.console_port).map(Arc::new);
    if let Some(f) = &fold {
        let _ = foldable.set(f.clone());
    }
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
            audio: inst.audio_tap.clone(),
            fold: fold.clone(),
            rotation_unknown: instance::rotation_unknown(&avd),
        },
    )?;
    let _advert = console::Advertisement::write(&avd.name, &avd.dir, inst.console_port);
    // Live display + input for the desktop app (<avd>/simee/display.sock).
    let _display = inst.display.clone().and_then(|d| display_server::start(&avd.dir, d, inst.inputs.clone(), stop.clone()));
    // On stdout: launchers watch for the first line and the `emulator-<port>` serial.
    println!("emu: {} on emulator-{} (adb port {})", avd.name, inst.console_port, inst.console_port + 1);
    let mut cfg = inst.machine_config(opts, launched)?;
    announce_fold_at_boot(&mut cfg, fold.clone());
    // The cheap checks passed (console port bound, the boot files exist) and `prepare` has set
    // the renderer's environment: bring the renderer up beside kernel load and VM setup. Nothing
    // fallible runs between here and `run`, which claims it (or, failing before the GPU, waits
    // for it to finish) before it returns.
    if let Some(gpu) = inst.gpu_spec(opts) {
        emu_vmm::start_renderer(gpu);
    }
    loop {
        let out = emu_vmm::run(cfg, stop.clone())?;
        if !matches!(out.reason, emu_vmm::StopReason::Reset) || stop.load(Ordering::SeqCst) {
            // Power-off, a signal, `kill` or a fault: write out what is queued and finish a WAV's sizes.
            inst.audio_tap.stop();
        }
        if opts.verbose {
            eprintln!("emu: stopped after {:.1}s: {:?}", out.uptime.as_secs_f64(), out.reason);
            for line in exit_lines(&out.exits) {
                eprintln!("emu: {line}");
            }
            if let Some(spool) = &inst.logcat {
                eprintln!("emu:   logcat bytes dropped (its output fell behind): {}", spool.dropped());
            }
        }
        if !matches!(out.reason, emu_vmm::StopReason::Reset) || stop.load(Ordering::SeqCst) {
            // What runs from here to the exit: instance state, the renderer's static teardown.
            emu_vmm::teardown_step(Some(EXITING), opts.verbose);
        }
        match out.reason {
            emu_vmm::StopReason::Reset if !stop.load(Ordering::SeqCst) => {
                emu_vmm::teardown_step(None, opts.verbose);
                // The guest boots again in its natural orientation, and logs a rotation only
                // when it next makes one.
                if let Some(display) = &inst.display {
                    display.set_rotation(sim_core::rotation::Rotation::Deg0);
                }
                cfg = inst.machine_config(opts, launched)?;
                announce_fold_at_boot(&mut cfg, fold.clone());
            }
            emu_vmm::StopReason::Fault(msg) => {
                eprintln!("emu: guest fault: {msg}");
                return Ok(ExitCode::FAILURE);
            }
            // 130 or 143 when a signal stopped it.
            _ => return Ok(on_signal::stopped()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn startup_refuses_below_the_android_floor_and_names_what_is_off() {
        use sim_core::support::Version;
        let e = android_support(Some(Version::new(14, 6, 1))).expect_err("below 15");
        assert!(e.contains("needs macOS 15") && e.contains("hv_gic_create") && e.contains("runs macOS 14.6.1"), "{e}");
        let off = android_support(Some(Version::new(15, 4, 0))).expect("15 boots");
        assert_eq!(off.len(), 1, "{off:?}");
        assert!(off[0].starts_with("4 KiB guest memory granule needs macOS 26"), "{}", off[0]);
        assert_eq!(android_support(Some(Version::new(26, 0, 0))), Ok(vec![]));
        assert_eq!(android_support(None), Ok(vec![]), "unknown: the hypervisor itself says");
        if cfg!(target_os = "macos") {
            assert!(host_macos().is_some_and(|v| v.major >= 11));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_open_file_limit_is_raised_toward_the_hard_limit_and_never_lowered() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let was = getrlimit(Resource::Nofile);
        let hard = was.maximum.unwrap_or(u64::MAX);
        let want = hard.min(NOFILE_WANT);
        // From a low soft limit (macOS's default is 256; 1024 leaves the other tests room).
        let low = want.min(1024);
        setrlimit(Resource::Nofile, Rlimit { current: Some(low), maximum: was.maximum }).expect("lower");
        let raised = raise_nofile();
        let now = getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX);
        setrlimit(Resource::Nofile, was).expect("restore");
        assert_eq!(raised, want);
        assert_eq!(now, want, "the process's limit, not only the return value");
        // Already higher (a shell's `ulimit -n`): left alone.
        let soft = was.current.unwrap_or(u64::MAX);
        if soft >= want {
            assert_eq!(raise_nofile(), soft);
        }
    }

    #[test]
    fn an_overrun_stop_names_its_step_and_keeps_the_exit_counts() {
        let line = overrun_line(Some(("releasing virtio-gpu".into(), Duration::from_millis(4200))));
        assert_eq!(line, "shutdown took over 5 s (releasing virtio-gpu, for 4.2 s); exiting");
        assert!(overrun_line(None).contains("had not started tearing down"));
        let exits = vec![vec![("mmio_read", 3), ("wfi", 0), ("canceled", 1)], vec![("mmio_read", 0)]];
        assert_eq!(exit_lines(&exits), ["  vcpu0: mmio_read=3 canceled=1", "  vcpu1: "]);
    }
}
