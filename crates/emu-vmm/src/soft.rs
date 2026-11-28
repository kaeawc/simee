//! Software mode (#55): an x86_64 guest whose CPU runs in QEMU, with every device but the disks
//! served by emu. QEMU runs the CPU on the host's KVM when `/dev/kvm` is usable ([`Accel::Kvm`]),
//! and in its TCG otherwise.
//!
//! QEMU gets stock's x86 board (`pc`, virtio-blk-pci at 00:03.0–00:07.0, virtio-rng at 00:08.0)
//! and attaches emu's devices: vsock, console, net, input and the GPU over vhost-user (QEMU's
//! generic `vhost-user-test-device-pci` for all but vsock), and the goldfish address space and pipe
//! over vfio-user, the pipe described by an ACPI table. Guest RAM is a shared memfd that QEMU and
//! emu both map. [`qemu_args`] decides the command line with no side effects; [`run`] serves the
//! devices, starts QEMU and supervises it until the guest powers off or a stop is requested. QEMU
//! resets the machine itself on a guest reboot, so one [`run`] covers every boot of the process.
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::disk::{DiskMode, DiskSpec};
use crate::layout::x86::{PIPE_SLOT, RNG_SLOT, disk_slot};
use crate::{devices, pipe, vfio_user, vhost_user, virtio};

/// QEMU's guest CPU: everything TCG offers but the fast-string features, which steer the guest
/// kernel's memcpy to `rep movsb`, a byte loop under TCG (md5 51% slower, zygote 10% later;
/// docs/benchmarks.md).
pub const CPU: &str = "max,-erms,-fsrm,-fzrm,-fsrs,-fsrc";

/// What runs the guest's CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// QEMU's multi-threaded TCG with [`CPU`]: any host.
    Tcg,
    /// The host's KVM with the host's CPU model: an x86_64 Linux host with a usable `/dev/kvm`.
    Kvm,
}

/// The oldest QEMU software mode accepts: 11.1, the release it was built and measured on (vhost-user
/// `SHMEM_MAP` for GPU blobs, vfio-user, memory slots). 6.2's multi-threaded TCG panics these
/// guests within seconds.
pub const MIN_QEMU: (u32, u32) = (11, 1);

/// `QEMU emulator version 11.1.3 (...)` → (11, 1).
pub fn parse_version(out: &str) -> Option<(u32, u32)> {
    let rest = out.split("version ").nth(1)?;
    let mut it = rest.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// The QEMU to run: `EMU_QEMU`, else `qemu-system-x86_64` on `path` (a `PATH` value).
pub fn find_qemu(env_qemu: Option<&str>, path: &str) -> Option<PathBuf> {
    if let Some(q) = env_qemu.filter(|q| !q.is_empty()) {
        return Some(PathBuf::from(q));
    }
    path.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("qemu-system-x86_64")).find(|p| p.is_file())
}

/// Checks that `qemu` runs and is new enough; its version line otherwise says what's wrong.
pub fn check_qemu(qemu: &Path) -> anyhow::Result<(u32, u32)> {
    let out = Command::new(qemu).arg("-version").output().map_err(|e| anyhow::anyhow!("running {}: {e}", qemu.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = parse_version(&text).ok_or_else(|| anyhow::anyhow!("{} -version printed no version: {text}", qemu.display()))?;
    if v < MIN_QEMU {
        anyhow::bail!(
            "{} is QEMU {}.{}; software mode needs {}.{} or newer (older multi-threaded TCG panics these guests)",
            qemu.display(),
            v.0,
            v.1,
            MIN_QEMU.0,
            MIN_QEMU.1
        );
    }
    Ok(v)
}

/// One device emu serves to QEMU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// vhost-user through QEMU's generic frontend: (name, virtio id, queues, config size).
    Generic(String, u32, usize, usize),
    /// vhost-user vsock (QEMU's own frontend: the guest CID is its property).
    Vsock,
    /// vfio-user at an optional PCI slot: (name, slot).
    Vfio(String, Option<u8>),
}

/// What [`qemu_args`] needs to know.
pub struct Board<'a> {
    pub cpus: u32,
    pub ram_mb: u64,
    pub kernel: &'a Path,
    pub initrd: &'a Path,
    pub cmdline: &'a str,
    pub disks: &'a [DiskSpec],
    /// fsync persistent disks when the guest flushes.
    pub disk_fsync: bool,
    /// Where the sockets and the ACPI table are.
    pub dir: &'a Path,
    pub served: &'a [Served],
    /// The goldfish pipe's SSDT, with graphics.
    pub acpi_table: Option<&'a Path>,
    pub accel: Accel,
}

/// QEMU's command line (after the binary) for `board`.
pub fn qemu_args(b: &Board) -> Vec<String> {
    let (accel, cpu) = match b.accel {
        Accel::Tcg => ("tcg,thread=multi", CPU),
        Accel::Kvm => ("kvm", "host"),
    };
    let mut a: Vec<String> =
        ["-nodefaults", "-no-user-config", "-display", "none", "-M", "pc,memory-backend=mem", "-accel", accel, "-cpu", cpu]
            .map(String::from)
            .to_vec();
    a.extend(["-smp".into(), b.cpus.to_string(), "-m".into(), format!("{}M", b.ram_mb)]);
    // Shared, so emu maps the same pages (vhost-user's memory table, vfio-user's DMA).
    a.extend(["-object".into(), format!("memory-backend-memfd,id=mem,size={}M,share=on", b.ram_mb)]);
    a.extend(["-kernel".into(), b.kernel.display().to_string(), "-initrd".into(), b.initrd.display().to_string()]);
    a.extend(["-append".into(), b.cmdline.to_string()]);
    // The kernel console on QEMU's stdout, which emu copies where the console log goes.
    a.extend(["-chardev".into(), "stdio,id=ser,signal=off".into(), "-serial".into(), "chardev:ser".into()]);
    for d in b.disks {
        let Some(slot) = disk_slot(&d.serial) else { continue };
        let cache = if b.disk_fsync { "writeback" } else { "unsafe" };
        let snapshot = if d.mode == DiskMode::Ephemeral { ",snapshot=on" } else { "" };
        a.extend([
            "-drive".into(),
            format!("file={},if=none,id={},format=raw,cache={cache},aio=threads{snapshot}", d.path.display(), d.serial),
            "-device".into(),
            format!("virtio-blk-pci,drive={},addr={slot}", d.serial),
        ]);
    }
    a.extend(["-device".into(), format!("virtio-rng-pci,addr={RNG_SLOT}")]);
    for s in b.served {
        match s {
            Served::Generic(name, id, queues, config) => a.extend([
                "-chardev".into(),
                format!("socket,id={name},path={}", b.dir.join(format!("{name}.sock")).display()),
                "-device".into(),
                format!("vhost-user-test-device-pci,chardev={name},virtio-id={id},num_vqs={queues},config_size={config}"),
            ]),
            Served::Vsock => a.extend([
                "-chardev".into(),
                format!("socket,id=vsock,path={}", b.dir.join("vsock.sock").display()),
                "-device".into(),
                "vhost-user-vsock-pci,chardev=vsock".into(),
            ]),
            Served::Vfio(name, slot) => {
                a.extend(["-device".into(), vfio_user::qemu_device(&b.dir.join(format!("{name}.sock")), *slot)]);
            }
        }
    }
    if let Some(t) = b.acpi_table {
        a.extend(["-acpitable".into(), format!("file={}", t.display())]);
        // The 64-bit window holds the GPU's 8 GiB region and the 16 GiB address-space area.
        a.extend(["-global".into(), "i440FX-pcihost.pci-hole64-size=64G".into()]);
    }
    a
}

/// virtio-input's config: select, subsel, size, 5 reserved bytes and a 128-byte union.
const INPUT_CONFIG: usize = 0x88;

/// The size of a device's config space as QEMU's generic frontend needs it up front: the last
/// offset that reads non-zero, rounded up to 4 bytes. virtio-input's reads as zeros until the
/// guest selects what to read, so it's given in full.
pub fn config_size(dev: &dyn virtio::VirtioDevice) -> usize {
    if dev.device_id() == virtio::input::ID_INPUT {
        return INPUT_CONFIG;
    }
    let mut buf = [0u8; 256];
    dev.read_config(0, &mut buf);
    buf.iter().rposition(|&b| b != 0).map_or(0, |i| (i + 1).next_multiple_of(4))
}

/// The GPU, as software mode runs it: gfxstream from `lib64` on software Vulkan, Vulkan only.
pub struct Gpu {
    pub lib64: PathBuf,
    pub icd: emu_gfxstream::Icd,
    pub width: u32,
    pub height: u32,
    pub display: Arc<crate::display::FrameStore>,
    pub verbose: bool,
}

pub struct Config {
    pub qemu: PathBuf,
    /// Sockets, the initrd and the ACPI table: short (unix socket paths are limited to 108
    /// bytes) and private to this process.
    pub dir: PathBuf,
    pub cpus: u32,
    pub ram_bytes: u64,
    pub kernel: PathBuf,
    /// Initrd bytes, bootconfig already appended.
    pub initrd: Vec<u8>,
    pub cmdline: String,
    pub disks: Vec<DiskSpec>,
    pub disk_fsync: bool,
    pub adb: Arc<pipe::adb::AdbListener>,
    pub gpu: Option<Gpu>,
    pub boot_props: Vec<(String, String)>,
    pub on_guest_event: Option<pipe::misc::GuestEvent>,
    pub sensors: Arc<pipe::sensors::Sensors>,
    pub clipboard: Arc<pipe::clipboard::Clipboard>,
    pub fingerprint: Arc<pipe::fingerprint::Fingerprint>,
    pub gps: Arc<pipe::gps::Gps>,
    pub telephony: Arc<crate::telephony::Telephony>,
    pub network: Arc<Mutex<sim_core::network::Conditions>>,
    pub car: Option<Arc<pipe::car::Car>>,
    pub xr: Option<Arc<pipe::xr::Xr>>,
    /// Touchscreen size; enables the touchscreen, keyboard and rotary devices.
    pub input: Option<(u32, u32)>,
    pub touch_screen: bool,
    pub on_input: Option<Arc<dyn Fn(virtio::input::Inputs) + Send + Sync>>,
    /// Where the kernel console goes.
    pub console: Box<dyn Write + Send>,
    pub logcat: Option<virtio::console::Spool>,
    pub verbose: bool,
    pub accel: Accel,
    /// `accel` was chosen automatically: a QEMU that fails early on KVM is started again on TCG
    /// ([`retry_on_tcg`]).
    pub accel_fallback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// The guest powered off (QEMU exited by itself, status 0).
    PowerOff,
    /// A stop was requested and QEMU ended.
    Stopped,
    /// QEMU failed: its status and last line on stderr.
    Failed(String),
}

/// QEMU warnings that say nothing about this guest: vfio-user has no error-recovery IRQ to offer.
fn benign(line: &str) -> bool {
    line.contains("Could not enable error recovery for the device")
}

/// Serves the devices, starts QEMU, and returns when it ends (or, after `stop` is set, once it
/// has been asked to quit and has).
pub fn run(cfg: Config, stop: Arc<AtomicBool>) -> anyhow::Result<Exit> {
    use anyhow::Context;
    std::fs::create_dir_all(&cfg.dir).with_context(|| format!("creating {}", cfg.dir.display()))?;
    let initrd = cfg.dir.join("initrd");
    std::fs::write(&initrd, &cfg.initrd).context("writing the initrd")?;
    let mut served = Vec::new();

    // The goldfish address space and the renderer first: `refcount` frees color buffers in the
    // renderer, and the renderer's DMA reaches into the area.
    let gas = cfg.gpu.as_ref().map(|_| vfio_user::GoldfishAs::new(crate::layout::GOLDFISH_AS_AREA_SIZE)).transpose()?;
    let gpu = match (&cfg.gpu, &gas) {
        (Some(spec), Some(gas)) => Some(gpu(spec, gas.area(), gas.guest_ram_handle())?),
        _ => None,
    };

    let (events, rx) = virtio::vsock::event_channel();
    let registry = Arc::new(registry(&cfg, events.clone(), gpu.as_ref().map(|g| g.renderer.clone())));
    let vsock = virtio::vsock::Vsock::new(3, registry.clone(), events, rx);
    vhost_user::serve(&cfg.dir.join("vsock.sock"), "vsock", Box::new(vsock))?;
    served.push(Served::Vsock);

    let mut devices: Vec<(String, Box<dyn virtio::VirtioDevice>, vhost_user::BackendChannel)> = Vec::new();
    devices.push(("console".into(), Box::new(console(&cfg)), Default::default()));
    devices.push(("net".into(), Box::new(virtio::net::Net::new(net(&cfg))), Default::default()));
    if let Some((w, h)) = cfg.input {
        use virtio::input::{Input, Inputs, Kind};
        let touch = cfg.touch_screen.then(|| {
            let (dev, handle) = Input::new(Kind::Touch { width: w, height: h, screen: true }, "virtio_input_multi_touch_1");
            devices.push(("touch".into(), Box::new(dev), Default::default()));
            handle
        });
        let (kbd, keyboard) = Input::new(Kind::Keyboard, virtio::input::KEYBOARD_NAME);
        let (rot, rotary) = Input::new(Kind::Rotary, "virtio_input_rotary");
        devices.push(("keyboard".into(), Box::new(kbd), Default::default()));
        devices.push(("rotary".into(), Box::new(rot), Default::default()));
        if let Some(cb) = &cfg.on_input {
            cb(Inputs { screen: (w, h), touch, keyboard, rotary, mouse: None, touchpad: None });
        }
    }
    let mut acpi_table = None;
    if let (Some(g), Some(gas)) = (gpu, gas) {
        devices.push(("gpu".into(), g.device, g.channel));
        vfio_user::serve(&cfg.dir.join("goldfish-as.sock"), "goldfish-as", Box::new(gas))?;
        served.push(Served::Vfio("goldfish-as".into(), None));
        let carrier = vfio_user::GoldfishPipeCarrier::new(registry.clone());
        vfio_user::serve(&cfg.dir.join("goldfish-pipe.sock"), "goldfish-pipe", Box::new(carrier))?;
        served.push(Served::Vfio("goldfish-pipe".into(), Some(PIPE_SLOT)));
        let table = cfg.dir.join("goldfish-pipe.aml");
        std::fs::write(&table, crate::acpi::goldfish_pipe_ssdt(PIPE_SLOT, crate::layout::GOLDFISH_PIPE_SIZE as u32))?;
        acpi_table = Some(table);
    }
    for (name, dev, chan) in devices {
        let (id, queues, config) = (dev.device_id(), dev.num_queues(), config_size(dev.as_ref()));
        vhost_user::serve_with(&cfg.dir.join(format!("{name}.sock")), &name, dev, chan)?;
        served.push(Served::Generic(name, id, queues, config));
    }

    // Shared by every QEMU this run starts (a TCG retry after KVM fails).
    let console = Arc::new(Mutex::new(cfg.console));
    let mut accel = cfg.accel;
    loop {
        let args = qemu_args(&Board {
            cpus: cfg.cpus,
            ram_mb: cfg.ram_bytes >> 20,
            kernel: &cfg.kernel,
            initrd: &initrd,
            cmdline: &cfg.cmdline,
            disks: &cfg.disks,
            disk_fsync: cfg.disk_fsync,
            dir: &cfg.dir,
            served: &served,
            acpi_table: acpi_table.as_deref(),
            accel,
        });
        if cfg.verbose {
            eprintln!("emu: {} {}", cfg.qemu.display(), args.join(" "));
        }
        let started = Instant::now();
        let mut child = Command::new(&cfg.qemu)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", cfg.qemu.display()))?;
        pipe::adb::AdbService::announce_port(cfg.adb.port);
        let console_out = copy_console(child.stdout.take(), console.clone());
        let last_err = forward_stderr(child.stderr.take(), cfg.verbose);
        let exit = supervise(&mut child, &stop);
        let guest_output = console_out.and_then(|t| t.join().ok()).unwrap_or(0) > 0;
        let err = last_err.and_then(|t| t.join().ok()).unwrap_or_default();
        let last = err.last.clone();
        let exit = match exit? {
            Some(status) if status.success() => Exit::PowerOff,
            Some(_) if stop.load(Ordering::SeqCst) => Exit::Stopped,
            None => Exit::Stopped,
            Some(status) => Exit::Failed(format!("QEMU {status}{}", last.map(|l| format!(": {l}")).unwrap_or_default())),
        };
        let seen = Failure { kvm_named: err.kvm_named, said_anything: err.said_anything, guest_output };
        if retry_on_tcg(cfg.accel_fallback, accel, &exit, started.elapsed(), seen) {
            if let Exit::Failed(why) = &exit {
                eprintln!("emu: {why} on KVM; starting again on TCG (-accel off skips KVM)");
            }
            accel = Accel::Tcg;
            continue;
        }
        let _ = std::fs::remove_dir_all(&cfg.dir);
        return Ok(exit);
    }
}

/// How long QEMU may have run on KVM and still be retried on TCG when it fails: a backstop to
/// [`Failure`]'s evidence, since failing to set up KVM (another hypervisor holding VT-x, a QEMU
/// built without KVM) ends QEMU before the guest runs.
pub const KVM_EARLY_FAILURE: Duration = Duration::from_secs(10);

/// What a failed QEMU left behind, for [`retry_on_tcg`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Failure {
    /// A line on QEMU's stderr named KVM or the accelerator ([`names_kvm`]).
    pub kvm_named: bool,
    /// QEMU printed anything on stderr.
    pub said_anything: bool,
    /// The guest wrote to its console: it ran.
    pub guest_output: bool,
}

/// Whether a QEMU stderr line is about KVM or the accelerator: `invalid accelerator kvm`,
/// `Could not access KVM kernel module`, `failed to initialize kvm: Device or resource busy`,
/// `kvm_init_vcpu: ...`.
pub fn names_kvm(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("kvm") || l.contains("accelerator")
}

/// Whether a QEMU that ended with `exit` after `ran` on `accel` is started again on TCG: only
/// when the accelerator was chosen automatically (`fallback`), it was KVM, QEMU failed within
/// [`KVM_EARLY_FAILURE`], and the failure is KVM's: its stderr named KVM or the accelerator, or it
/// died silently before the guest wrote anything. Any other failure (a bad disk, a device socket)
/// is reported as it is, without a TCG attempt that would fail the same way; so is a guest that
/// ran, or a stop.
pub fn retry_on_tcg(fallback: bool, accel: Accel, exit: &Exit, ran: Duration, seen: Failure) -> bool {
    let kvms = seen.kvm_named || (!seen.said_anything && !seen.guest_output);
    fallback && accel == Accel::Kvm && matches!(exit, Exit::Failed(_)) && ran < KVM_EARLY_FAILURE && kvms
}

/// Waits for QEMU; once `stop` is set, asks it to quit (SIGTERM, which QEMU handles by shutting
/// down) and, if it hasn't within 10 s, kills it. None: it was stopped by force.
fn supervise(child: &mut Child, stop: &AtomicBool) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut asked: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        match asked {
            None if stop.load(Ordering::SeqCst) => {
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
                }
                asked = Some(Instant::now());
            }
            Some(t) if t.elapsed() > Duration::from_secs(10) => {
                child.kill()?;
                child.wait()?;
                return Ok(None);
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Copies the guest console to `to`; the thread's result is how many bytes the guest wrote.
fn copy_console(from: Option<std::process::ChildStdout>, to: Arc<Mutex<Box<dyn Write + Send>>>) -> Option<std::thread::JoinHandle<u64>> {
    let mut from = from?;
    std::thread::Builder::new()
        .name("qemu-console".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            let mut total = 0u64;
            while let Ok(n) = from.read(&mut buf) {
                let Ok(mut to) = to.lock() else { break };
                if n == 0 || to.write_all(&buf[..n]).is_err() {
                    break;
                }
                total += n as u64;
                let _ = to.flush();
            }
            total
        })
        .ok()
}

/// What QEMU said on stderr.
#[derive(Debug, Default)]
struct Stderr {
    /// The last line that isn't [`benign`] (any line with `verbose`).
    last: Option<String>,
    /// Some line, benign or not, [`names_kvm`].
    kvm_named: bool,
    said_anything: bool,
}

/// QEMU's stderr on emu's (all of it with `verbose`, else what isn't [`benign`]).
fn forward_stderr(from: Option<std::process::ChildStderr>, verbose: bool) -> Option<std::thread::JoinHandle<Stderr>> {
    let from = from?;
    std::thread::Builder::new()
        .name("qemu-stderr".into())
        .spawn(move || {
            let mut seen = Stderr::default();
            for line in BufReader::new(from).lines().map_while(Result::ok) {
                seen.said_anything = true;
                seen.kvm_named |= names_kvm(&line);
                if verbose || !benign(&line) {
                    eprintln!("emu: qemu: {line}");
                    seen.last = Some(line);
                }
            }
            seen
        })
        .ok()
}

/// The host services the guest opens by name, over vsock and the goldfish pipe: what emu's own
/// machine serves (`machine.rs`), less what this board has no device for.
fn registry(
    cfg: &Config,
    events: std::sync::mpsc::Sender<pipe::WorkerEvent>,
    renderer: Option<Arc<emu_gfxstream::Renderer>>,
) -> pipe::PipeRegistry {
    let mut r = pipe::PipeRegistry::default();
    r.register("qemud:adb", Box::new(cfg.adb.attach(events)));
    r.register("qemud:boot-properties", Box::new(pipe::qemud::BootProperties { props: cfg.boot_props.clone() }));
    let on_last_ref = renderer.map(|r| -> devices::goldfish_pipe::OnLastRef {
        Arc::new(move |handle| {
            let _ = r.release_color_buffer(handle);
        })
    });
    r.register("refcount", Box::new(devices::goldfish_pipe::RefCount { on_last_ref }));
    r.register("GLProcessPipe", Box::new(devices::goldfish_pipe::GlProcess::default()));
    r.register("QemuMiscPipe", Box::new(pipe::misc::MiscService { on_event: cfg.on_guest_event.clone() }));
    r.register("qemud:sensors", Box::new(pipe::sensors::SensorService { sensors: cfg.sensors.clone() }));
    r.register("clipboard", Box::new(pipe::clipboard::ClipboardService { clipboard: cfg.clipboard.clone() }));
    r.register("qemud:fingerprintlisten", Box::new(pipe::fingerprint::FingerprintService { fingerprint: cfg.fingerprint.clone() }));
    r.register("qemud:gps", Box::new(pipe::gps::GpsService { gps: cfg.gps.clone() }));
    if let Some(xr) = &cfg.xr {
        r.register("qemud:xr_service", Box::new(pipe::xr::XrService { xr: xr.clone() }));
    }
    if let Some(car) = &cfg.car {
        r.register("qemud:car", Box::new(pipe::car::CarService { car: car.clone() }));
    }
    r.register("multidisplay", Box::new(pipe::misc::Silent("multidisplay")));
    r.register("qemud:camera", Box::new(pipe::camera::CameraService::new(None)));
    r
}

/// The virtio-console ports, as emu's own machine has them: hvc0, the modem, logcat's hvc1, UWB
/// and Bluetooth.
fn console(cfg: &Config) -> virtio::console::Console {
    use virtio::console::{Console, PortBackend, PortSpec, Sink, Writer};
    let logcat: Box<dyn PortBackend> = match cfg.logcat.clone() {
        Some(w) => Box::new(Writer(w)),
        None => Box::new(Sink),
    };
    let address = cfg.disks.iter().map(|d| d.path.as_os_str().as_encoded_bytes()).collect::<Vec<_>>().concat();
    let (console, handles) = Console::new(vec![
        PortSpec { name: String::new(), backend: Box::new(Sink) },
        PortSpec { name: "modem".into(), backend: cfg.telephony.backend() },
        PortSpec { name: String::new(), backend: logcat },
        PortSpec { name: "uwb".into(), backend: Box::new(crate::bt::UwbPort::default()) },
        PortSpec { name: "bluetooth".into(), backend: Box::new(crate::bt::BluetoothPort::new(crate::bt::address_for(&address))) },
    ]);
    if let Some(h) = handles.get(1) {
        cfg.telephony.attach(h.clone());
    }
    console
}

fn net(cfg: &Config) -> Box<dyn virtio::net::Backend> {
    if std::env::var("EMU_NET").is_ok_and(|v| v == "link") {
        return Box::new(virtio::net::LinkOnly);
    }
    match crate::net::Nat::new(crate::net::Config { network: cfg.network.clone(), ..Default::default() }) {
        Ok(n) => Box::new(n),
        Err(e) => {
            eprintln!("emu: net: no NAT ({e}); the network is link only");
            Box::new(virtio::net::LinkOnly)
        }
    }
}

struct GpuParts {
    device: Box<dyn virtio::VirtioDevice>,
    channel: vhost_user::BackendChannel,
    renderer: Arc<emu_gfxstream::Renderer>,
}

/// The renderer on software Vulkan, Vulkan only, with blobs it can export (QEMU maps them into the
/// GPU's shared-memory region), its DMA reaching guest RAM and the address-space area.
fn gpu(spec: &Gpu, area: vfio_user::AreaHandle, ram: vfio_user::AreaHandle) -> anyhow::Result<GpuParts> {
    use emu_gfxstream::{Renderer, flags};
    use virtio::gpu::{GpuConfig, fence_handler};

    use crate::display::DisplaySink;
    let dir = spec.lib64.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent", spec.lib64.display()))?;
    for n in emu_gfxstream::configure_environment(dir, spec.icd) {
        eprintln!("{n}");
    }
    let r = Arc::new(Renderer::load(&spec.lib64)?);
    let ticker = crate::display::PostTicker::start(
        spec.display.clone(),
        spec.width,
        spec.height,
        Duration::from_millis(16),
        Duration::from_millis(50),
    );
    let fences = fence_handler();
    let on_fence: Arc<dyn Fn(emu_gfxstream::Fence) + Send + Sync> = Arc::new(move |f| {
        fences(f);
        ticker.poke();
    });
    let features = "RefCountPipe:enabled,GlDirectMem:enabled,HasSharedSlotsHostMemoryAllocator:enabled,VulkanVirtualQueue:enabled";
    let rflags = flags::USE_EGL | flags::USE_VK | flags::VULKAN_NATIVE_SWAPCHAIN | flags::USE_EXTERNAL_BLOB;
    r.init_with_features(spec.width, spec.height, rflags, Some(features), spec.verbose, on_fence)?;
    let translate: emu_gfxstream::DmaTranslator = Box::new(move |gpa| {
        let hit = |h: &vfio_user::AreaHandle| h.get().and_then(|m| m.host_range(gpa, 1));
        hit(&ram).or_else(|| hit(&area)).map_or(std::ptr::null_mut(), |h| h.as_ptr())
    });
    r.set_dma_translator(Some(translate))?;
    let reader = r.clone();
    spec.display.set_readback(Box::new(move |w, h, buf| reader.read_last_posted_to(w, h, buf).is_ok()));
    let channel = vhost_user::BackendChannel::default();
    let window = Arc::new(vhost_user::ShmemWindow::new(channel.clone(), virtio::gpu::SHM_HOST_VISIBLE, r.clone()));
    let device = virtio::gpu::Gpu::new(GpuConfig {
        width: spec.width,
        height: spec.height,
        renderer: r.clone(),
        shm: (0, 8 << 30),
        window,
        display: spec.display.clone(),
        verbose: spec.verbose,
    });
    Ok(GpuParts { device: Box::new(device), channel, renderer: r })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disks() -> Vec<DiskSpec> {
        ["system", "cache", "userdata", "metadata", "vendor"]
            .iter()
            .enumerate()
            .map(|(i, s)| DiskSpec {
                slot: i as u32,
                path: PathBuf::from(format!("/d/{s}.img")),
                mode: if matches!(*s, "system" | "vendor") { DiskMode::Ephemeral } else { DiskMode::Persistent },
                serial: (*s).to_string(),
            })
            .collect()
    }

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn the_board_is_stocks_x86_board() {
        let disks = disks();
        let args = qemu_args(&Board {
            cpus: 4,
            ram_mb: 2048,
            kernel: Path::new("/i/kernel-ranchu"),
            initrd: Path::new("/r/initrd"),
            cmdline: "console=ttyS0",
            disks: &disks,
            disk_fsync: false,
            dir: Path::new("/r"),
            served: &[],
            acpi_table: None,
            accel: Accel::Tcg,
        });
        let line = joined(&args);
        // Stock's slots: system 3, cache 4, userdata 5, metadata 6, vendor 7, rng 8.
        for (serial, slot) in [("system", 3), ("cache", 4), ("userdata", 5), ("metadata", 6), ("vendor", 7)] {
            assert!(line.contains(&format!("virtio-blk-pci,drive={serial},addr={slot}")), "{serial}: {line}");
        }
        assert!(line.contains("virtio-rng-pci,addr=8"));
        // Image partitions discard writes; the AVD's own keep them.
        assert!(line.contains("file=/d/system.img,if=none,id=system,format=raw,cache=unsafe,aio=threads,snapshot=on"));
        assert!(line.contains("file=/d/userdata.img,if=none,id=userdata,format=raw,cache=unsafe,aio=threads -device"));
        // Multi-threaded TCG on a shared-memory RAM backend emu can map.
        assert!(line.contains("-accel tcg,thread=multi -cpu max,-erms,-fsrm,-fzrm,-fsrs,-fsrc -smp 4 -m 2048M"));
        assert!(line.contains("-M pc,memory-backend=mem"));
        assert!(line.contains("memory-backend-memfd,id=mem,size=2048M,share=on"));
        // `-append` is one argument however many spaces the command line has.
        let at = args.iter().position(|a| a == "-append").expect("append");
        assert_eq!(args[at + 1], "console=ttyS0");
        assert!(!line.contains("-acpitable"), "no pipe table without graphics");
    }

    #[test]
    fn served_devices_attach_through_their_sockets() {
        let disks = disks();
        let served = [Served::Vsock, Served::Generic("gpu".into(), 16, 2, 24), Served::Vfio("goldfish-pipe".into(), Some(PIPE_SLOT))];
        let line = joined(&qemu_args(&Board {
            cpus: 2,
            ram_mb: 1024,
            kernel: Path::new("/k"),
            initrd: Path::new("/r/initrd"),
            cmdline: "",
            disks: &disks,
            disk_fsync: true,
            dir: Path::new("/r"),
            served: &served,
            acpi_table: Some(Path::new("/r/goldfish-pipe.aml")),
            accel: Accel::Kvm,
        }));
        assert!(line.contains("-chardev socket,id=vsock,path=/r/vsock.sock -device vhost-user-vsock-pci,chardev=vsock"));
        assert!(line.contains(
            "-chardev socket,id=gpu,path=/r/gpu.sock -device vhost-user-test-device-pci,chardev=gpu,virtio-id=16,num_vqs=2,config_size=24"
        ));
        assert!(line.contains(r#""path":"/r/goldfish-pipe.sock""#) && line.contains(r#""addr":"1e""#));
        assert!(line.contains("-acpitable file=/r/goldfish-pipe.aml -global i440FX-pcihost.pci-hole64-size=64G"));
        assert!(line.contains("cache=writeback"), "-disk-cache fsync honours guest flushes");
        // KVM runs the host's CPU model: the fast-string features only cost under TCG.
        assert!(line.contains("-accel kvm -cpu host -smp 2 -m 1024M"), "{line}");
        assert!(line.contains("-M pc,memory-backend=mem"), "KVM keeps the shared RAM backend");
    }

    #[test]
    fn qemu_versions_and_where_qemu_is() {
        assert_eq!(parse_version("QEMU emulator version 11.1.3 (v11.1.3)\nCopyright"), Some((11, 1)));
        assert_eq!(parse_version("QEMU emulator version 6.2.0 (Debian 1:6.2+dfsg-2ubuntu6.24)"), Some((6, 2)));
        assert_eq!(parse_version("nothing"), None);
        assert!((6, 2) < MIN_QEMU && (11, 1) >= MIN_QEMU);

        let tmp = std::env::temp_dir().join(format!("emu-soft-qemu-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("b")).expect("mkdir");
        std::fs::write(tmp.join("b/qemu-system-x86_64"), b"").expect("write");
        let path = format!("/nonexistent:{}", tmp.join("b").display());
        assert_eq!(find_qemu(None, &path), Some(tmp.join("b/qemu-system-x86_64")));
        assert_eq!(find_qemu(Some("/opt/q"), &path), Some(PathBuf::from("/opt/q")), "EMU_QEMU wins");
        assert_eq!(find_qemu(None, "/nonexistent"), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_early_kvm_failure_in_auto_mode_is_retried_on_tcg() {
        let failed = Exit::Failed("QEMU exit status: 1: invalid accelerator kvm".into());
        let early = Duration::from_millis(300);
        let kvm = Failure { kvm_named: true, said_anything: true, guest_output: false };
        assert!(retry_on_tcg(true, Accel::Kvm, &failed, early, kvm));
        assert!(!retry_on_tcg(false, Accel::Kvm, &failed, early, kvm), "-accel on: KVM or fail");
        assert!(!retry_on_tcg(true, Accel::Tcg, &failed, early, kvm), "TCG failing has nowhere to fall back to");
        assert!(!retry_on_tcg(true, Accel::Kvm, &failed, KVM_EARLY_FAILURE, kvm), "the time bound is a backstop");
        assert!(!retry_on_tcg(true, Accel::Kvm, &Exit::Stopped, early, kvm));
        assert!(!retry_on_tcg(true, Accel::Kvm, &Exit::PowerOff, early, kvm));
    }

    #[test]
    fn a_failure_that_isnt_kvms_is_reported_without_a_tcg_attempt() {
        let failed = Exit::Failed("QEMU exit status: 1: Could not open '/d/userdata.img'".into());
        let early = Duration::from_millis(300);
        let disk = Failure { kvm_named: false, said_anything: true, guest_output: false };
        assert!(!retry_on_tcg(true, Accel::Kvm, &failed, early, disk), "QEMU said why, and it isn't KVM");
        let ran = Failure { kvm_named: false, said_anything: false, guest_output: true };
        assert!(!retry_on_tcg(true, Accel::Kvm, &failed, early, ran), "the guest ran");
        let silent = Failure::default();
        assert!(retry_on_tcg(true, Accel::Kvm, &failed, early, silent), "died silently before the guest ran: maybe KVM");
        let kvm_after_run = Failure { kvm_named: true, said_anything: true, guest_output: true };
        assert!(retry_on_tcg(true, Accel::Kvm, &failed, early, kvm_after_run), "named KVM: KVM's, wherever the guest got");
    }

    #[test]
    fn qemus_kvm_errors_are_recognized() {
        for line in [
            "qemu-system-x86_64: invalid accelerator kvm",
            "Could not access KVM kernel module: Permission denied",
            "qemu-system-x86_64: failed to initialize kvm: Device or resource busy",
            "qemu-system-x86_64: -accel kvm: failed to initialize kvm: No such file or directory",
        ] {
            assert!(names_kvm(line), "{line}");
        }
        assert!(!names_kvm("qemu-system-x86_64: -drive file=/d/userdata.img: Could not open '/d/userdata.img': No such file or directory"));
        assert!(!names_kvm("Failed to connect to '/r/gpu.sock': Connection refused"));
    }

    #[test]
    fn a_device_says_how_much_config_space_it_has() {
        let (kbd, _) = virtio::input::Input::new(virtio::input::Kind::Keyboard, "k");
        // Input's config reads as zeros until the guest selects something; QEMU needs all of it.
        assert_eq!(config_size(&kbd), 0x88);
    }

    #[test]
    fn only_vfio_users_missing_error_irq_is_quiet() {
        assert!(benign("qemu-system-x86_64: -device {...}: warning: vfio ...: Could not enable error recovery for the device"));
        assert!(!benign("qemu-system-x86_64: vhost-user: failed"));
    }
}
