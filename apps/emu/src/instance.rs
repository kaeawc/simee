//! Per-AVD runtime state: disk images under `<avd>/simee/`, the instance lock, boot config.
//!
//! The stock emulator's own files in the AVD directory (qcow2 overlays, snapshots) are never
//! touched, so the same AVD keeps working with both. What a boot involves is decided first, with
//! no side effects, as a [`BootPlan`] (`emu -avd NAME -dry-run` prints it on any host); the
//! [`Instance`] that creates the disks and runs the plan exists only on Apple silicon.
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
use std::fs::File;
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
use anyhow::{Context, Result, bail};
use emu_avd::{Avd, BootConfig};
use emu_vmm::layout::slots;
use emu_vmm::{DiskMode, DiskSpec};
use sim_core::platform::{AndroidOs, SubPlatform};

use crate::args::Opts;

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
const CACHE_BYTES: u64 = 66 << 20;

/// Kernel arguments the stock board passes for ranchu images.
const BASE_CMDLINE: &str = "8250.nr_uarts=1 no_timer_check console=ttyAMA0,38400 keep_bootcon earlyprintk=ttyAMA0 \
                            ndns=2 loop.max_part=7 printk.devkmsg=on bootconfig mac80211_hwsim.radios=0";

/// Kernel arguments stock's x86 board passes (software mode, #55), with the console on the first
/// 8250 UART instead of stock's `console=0`, as on arm64. The image's own `kernel_cmdline.txt`
/// adds the rest (`clocksource=pit` on x86_64).
///
/// Plus `kfence.sample_interval=0`. These kernels build KFENCE with static keys
/// (`CONFIG_KFENCE_STATIC_KEYS`), so every 500 ms its sampling flips a branch inside every slab
/// allocation, live-patching kernel text through an `int3`. Under QEMU's multi-threaded TCG
/// another vCPU can execute the breakpoint after the patch has finished. Guests panicked with
/// `int3` in `__kmem_cache_alloc_node`, three CPUs at once, and each flip also invalidates
/// QEMU's translations of that code. With hardware virtualization the CPU keeps that coherent,
/// so other boards keep KFENCE.
const X86_CMDLINE: &str = "8250.nr_uarts=1 no_timer_check console=ttyS0,38400 keep_bootcon ndns=2 loop.max_part=7 printk.devkmsg=on \
                           bootconfig mac80211_hwsim.radios=0 kfence.sample_interval=0";

/// Without `-show-kernel`, only messages below KERN_ERR reach the console (emergencies, alerts,
/// criticals, so init's FATAL lines). The PL011 traps twice per byte, and booting printed 230 KB
/// on it, 88% of all vCPU exits (~0.6 vCPU-s per boot, ~2.5 µs per byte in the writer, often
/// init). Stock (`SetupParameters.cpp`) passes `console=0` then and prints nothing; with
/// `-show-kernel` it passes `console=ttyAMA0,38400`, as simee always does. This keeps critical
/// lines in `console.log`, panics included (`panic()` raises the console level). One sysrq-c test
/// found console.log empty: each reset used to recreate the file, wiping the panic; it's opened
/// once per process now. Use `-show-kernel` or `EMU_CONSOLE=full` to debug boots; dmesg still has
/// everything.
const QUIET_CONSOLE: &str = "loglevel=3";

/// Every message on the console: `-show-kernel`, or `EMU_CONSOLE=full` (console.log only).
fn full_console(opts: &Opts) -> bool {
    opts.show_kernel || std::env::var("EMU_CONSOLE").is_ok_and(|v| v == "full")
}

/// The logcat filter the user asked for: `-logcat`'s, everything with `-logcat-output` alone,
/// else silent (`*:S`): the stream is discarded then (stock's null chardev).
fn user_logcat_filter(opts: &Opts) -> &str {
    match (&opts.logcat, &opts.logcat_output) {
        (Some(tags), _) => tags,
        (None, Some(_)) => "*:V",
        (None, None) => "*:S",
    }
}

/// Whether emu follows the guest's display rotation: on the form factors whose displays turn
/// ([`sim_core::rotation::turns`]), from the lines the guest logs when it rotates, on images with
/// a logcat console (bootconfig, API 31 and later). Images without one (API 29 and 30) aren't
/// followed, and their logcat filter stays the user's: it goes through `qemu.cmdline`, which holds
/// [`PROP_VALUE_MAX`] bytes, and the rotation tags would take the default past it (logcat then
/// shows every tag). There `rotation` says it doesn't know ([`rotation_unknown`]).
fn follows_rotation(avd: &Avd) -> bool {
    sim_core::rotation::turns(avd.profile().kind.form_factor) && takes_bootconfig(avd)
}

/// Whether the guest may turn its display without emu knowing: a display that turns on an image
/// whose rotation emu doesn't follow ([`follows_rotation`]). Frames and touches stay in panel
/// space there, and the console's `rotation` answers `unknown` rather than a turn it never read.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
pub fn rotation_unknown(avd: &Avd) -> bool {
    sim_core::rotation::turns(avd.profile().kind.form_factor) && !follows_rotation(avd)
}

/// The guest's logcat filter for `goldfish-logcat` (`logcat -f /dev/hvc1 ${ro.boot.logcat}`):
/// the user's ([`user_logcat_filter`]), plus, on a device whose display turns, the lines that
/// report its rotation ([`sim_core::rotation::ROTATION_TAGS`], a few per rotation or app start)
/// for emu to read. Anything else stays unformatted in the guest unless the user asked for it.
fn logcat_filter(avd: &Avd, opts: &Opts) -> String {
    let user = user_logcat_filter(opts);
    if !follows_rotation(avd) {
        return user.to_owned();
    }
    let parsed = sim_core::logcat::Filter::parse(user);
    let guest = sim_core::logcat::with_tags(&parsed, &sim_core::rotation::ROTATION_TAGS);
    if guest == parsed { user.to_owned() } else { guest.spec() }
}

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
pub struct Instance {
    avd: Avd,
    /// Console port (the `emulator-<port>` serial); adb listens on port + 1.
    pub console_port: u16,
    /// Latest guest frame, when host rendering is on.
    pub display: Option<std::sync::Arc<emu_vmm::display::FrameStore>>,
    gpu_lib64: Option<PathBuf>,
    /// Input handles of the current run, for the console and desktop.
    pub inputs: std::sync::Arc<std::sync::Mutex<Option<emu_vmm::Inputs>>>,
    /// Sensor values, shared by the guest's sensors HAL and the console.
    pub sensors: std::sync::Arc<emu_vmm::pipe::sensors::Sensors>,
    /// Guest clipboard, shared with the console.
    pub clipboard: std::sync::Arc<emu_vmm::pipe::clipboard::Clipboard>,
    pub fingerprint: std::sync::Arc<emu_vmm::pipe::fingerprint::Fingerprint>,
    pub gps: std::sync::Arc<emu_vmm::pipe::gps::Gps>,
    pub battery: std::sync::Arc<emu_vmm::devices::goldfish_battery::Battery>,
    pub telephony: std::sync::Arc<emu_vmm::telephony::Telephony>,
    /// What `network speed|delay` (and `-netspeed`/`-netdelay`) ask for: the console writes it,
    /// the guest's NAT shapes by it, and it survives a guest reset.
    pub network: std::sync::Arc<std::sync::Mutex<sim_core::network::Conditions>>,
    /// Android XR images: the guest's XR service link.
    pub xr: Option<std::sync::Arc<emu_vmm::pipe::xr::Xr>>,
    /// The vehicle HAL channel, for Automotive images only (as stock).
    pub car: Option<std::sync::Arc<emu_vmm::pipe::car::Car>>,
    /// The adb port, bound for the life of the process so a guest reset reboots on it.
    adb: std::sync::Arc<emu_vmm::pipe::adb::AdbListener>,
    _lock: Option<File>,
    /// This process's claim on its console port ([`claim_port`]).
    _port_claim: File,
    /// `<avd>/simee/console.log`, truncated when the process starts; every boot appends.
    console_log: File,
    /// Where logcat's stream goes (`-logcat`, `-logcat-output`), for the life of the process.
    pub logcat: Option<emu_vmm::virtio::console::Spool>,
}

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
impl Instance {
    pub fn prepare(avd: &Avd, opts: &Opts) -> Result<Instance> {
        let dir = emu_avd::state_dir(&avd.dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = if opts.read_only {
            None
        } else {
            let f = File::create(dir.join("instance.lock"))?;
            if f.try_lock().is_err() {
                bail!("AVD '{}' is already running (use -read-only to start another instance)", avd.name);
            }
            Some(f)
        };
        if opts.wipe_data {
            if opts.read_only {
                bail!("-wipe-data and -read-only can't be combined");
            }
            for f in ["userdata.img", "cache.img", "encryptionkey.img"] {
                let p = dir.join(f);
                if p.exists() {
                    std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
                }
            }
        }
        // User data starts as an empty ext4 filesystem (like stock): the guest mounts it, then
        // vold sets up metadata encryption in place. The metadata partition starts blank and is
        // formatted by the guest (`formattable` in the fstab). The ext4 formatter is native
        // (`emu-ext4`) and writes only metadata, so the image stays sparse.
        let userdata = dir.join("userdata.img");
        // A first boot, as stock tells one: no user data yet (a new AVD, or `-wipe-data`).
        let first_boot = !userdata.exists();
        if first_boot && let Err(e) = create_userdata(&userdata, avd.data_partition_bytes()) {
            let _ = std::fs::remove_file(&userdata);
            return Err(e);
        }
        emu_vmm::disk::ensure_sparse(&dir.join("cache.img"), CACHE_BYTES)?;
        let key_src = avd.sysdir.join("encryptionkey.img");
        if key_src.is_file() {
            let key_dst = dir.join("encryptionkey.img");
            if !key_dst.exists() {
                emu_avd::golden::clone_file_atomic(&key_src, &key_dst)?;
            }
        } else {
            emu_vmm::disk::ensure_sparse(&dir.join("encryptionkey.img"), 18 << 20)?;
        }
        // `<HOME>/Library/Caches/simee/ports/<port>.lock`, which launchers (sim-android's
        // allocator) also look at.
        let lock_dir = emu_avd::port_lock_dir().unwrap_or_else(|| dir.join("ports"));
        let (console_port, port_claim) = match opts.port {
            // An explicit port is claimed too: a second simee given the same one fails here,
            // clearly, instead of when it binds. Launchers match the message to try another pair.
            Some(p) => {
                let claim = claim_port(&lock_dir, p).with_context(|| format!("console port {p} {}", emu_avd::PORT_TAKEN))?;
                (p, claim)
            }
            None => pick_port(&lock_dir, PORTS, port_free).context("no free console/adb port pair in 5554..5682")?,
        };
        // Opened once per process, not per boot: a guest reset (a panic's reboot) keeps what
        // the boot before it wrote, panic text included.
        let console_log =
            File::create(dir.join("console.log")).with_context(|| format!("creating {}", dir.join("console.log").display()))?;
        let adb_port = console_port + 1;
        let adb = emu_vmm::pipe::adb::AdbListener::bind(adb_port).with_context(|| format!("binding adb port {adb_port}"))?;
        let gpu_lib64 = renderer(avd, opts);
        if gpu_lib64.is_none() && !gpu_off(opts) {
            eprintln!(
                "emu: no gfxstream renderer in the SDK (emulator/lib64 or simee/renderer); booting without graphics \
                 (install it with emu -provision-renderer)"
            );
        }
        if opts.verbose
            && let Some(lib64) = &gpu_lib64
        {
            eprintln!("emu: gfxstream renderer from {}", lib64.display());
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        if let Some(lib64) = &gpu_lib64
            && let Some(emulator_dir) = lib64.parent()
        {
            let choice = emu_avd::gpu::resolve(opts.gpu.as_deref(), avd.get("hw.gpu.mode"));
            if choice.guest_fallback {
                eprintln!("{}", emu_avd::gpu::GUEST_FALLBACK_NOTICE);
            }
            if let Some(n) = choice.notice {
                eprintln!("{n}");
            }
            let icd = match choice.backend {
                emu_avd::gpu::Backend::SwiftShader => emu_gfxstream::Icd::SwiftShader,
                emu_avd::gpu::Backend::Lavapipe => emu_gfxstream::Icd::Lavapipe,
                // The host GPU (MoltenVK, or KosmicKrisp for an API 35 UI on Skia GL on the
                // guest's ANGLE) unless MoltenVK fails on this machine; then software.
                _ => match crate::vkprobe::moltenvk(lib64) {
                    crate::vkprobe::Verdict::Pass => {
                        let asked = std::env::var("ANDROID_EMU_VK_ICD").ok();
                        let kosmickrisp = emu_gfxstream::Icd::KosmicKrisp.ships_in(lib64);
                        let picked = host_gpu_icd(avd, &guest_egl(avd), kosmickrisp, asked.as_deref());
                        let (icd, notice) = checked_host_icd(picked, || crate::vkprobe::kosmickrisp(lib64));
                        if let Some(n) = notice {
                            eprintln!("{n}");
                        } else if icd == emu_gfxstream::Icd::KosmicKrisp && opts.verbose && asked.is_none() {
                            eprintln!("{KOSMICKRISP_NOTICE}");
                        }
                        icd
                    }
                    crate::vkprobe::Verdict::Fail => {
                        eprintln!("emu: MoltenVK unavailable here; using lavapipe (software Vulkan)");
                        emu_gfxstream::Icd::Lavapipe
                    }
                    crate::vkprobe::Verdict::Unclear(why) => {
                        eprintln!("emu: MoltenVK's probe {why}; using lavapipe (software Vulkan) for this boot");
                        emu_gfxstream::Icd::Lavapipe
                    }
                },
            };
            // The launcher directory is the parent of lib64: the SDK's `emulator/` or
            // `<sdk>/simee/renderer/<revision>/`; gfxstream finds `lib64/vulkan` and
            // `lib64/gles_*` under it.
            emu_gfxstream::link_beside_exe(lib64);
            let without_gl = emu_vmm::runs_without_gl(guest_on_angle(avd), lib64);
            if let Some(platform) = host_angle(avd.api_level(), first_boot, without_gl) {
                if opts.verbose && std::env::var_os("ANGLE_DEFAULT_PLATFORM").is_none() {
                    let why = if without_gl { "the renderer runs without GL" } else { "API 29 first boot" };
                    eprintln!("emu: {why}: host GLES is ANGLE on {platform} (ANGLE_DEFAULT_PLATFORM=metal overrides)");
                }
                emu_gfxstream::prefer_angle_platform(platform);
            }
            // gfxstream gets the driver chosen here whatever ANDROID_EMU_VK_ICD said; a user's
            // value is reported honoured or overridden.
            for n in emu_gfxstream::configure_environment(emulator_dir, icd) {
                eprintln!("{n}");
            }
        }
        let display = gpu_lib64.as_ref().map(|_| std::sync::Arc::new(emu_vmm::display::FrameStore::default()));
        let watch = display.clone().filter(|_| follows_rotation(avd));
        let logcat =
            logcat_sink(opts, watch).with_context(|| format!("opening -logcat-output {}", opts.logcat_output.as_deref().unwrap_or("")))?;
        Ok(Instance {
            avd: avd.clone(),
            console_port,
            display,
            gpu_lib64,
            inputs: Default::default(),
            sensors: std::sync::Arc::new(emu_vmm::pipe::sensors::Sensors::new(avd.sensor_mask())),
            clipboard: Default::default(),
            fingerprint: Default::default(),
            gps: Default::default(),
            battery: Default::default(),
            telephony: Default::default(),
            network: std::sync::Arc::new(std::sync::Mutex::new(opts.network)),
            xr: is_xr(avd).then(Default::default),
            car: avd.is_car().then(Default::default),
            adb: std::sync::Arc::new(adb),
            _lock: lock,
            _port_claim: port_claim,
            console_log,
            logcat,
        })
    }

    pub fn plan(&self, opts: &Opts) -> BootPlan {
        BootPlan::new(&self.avd, opts, self.gpu_lib64.clone())
    }

    /// The GPU this boot gets, if any (`emu_vmm::start_renderer` takes it early, `run` again).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn gpu_spec(&self, opts: &Opts) -> Option<emu_vmm::GpuSpec> {
        self.gpu_lib64.as_ref().zip(self.display.as_ref()).map(|(lib64, display)| emu_vmm::GpuSpec {
            lib64: lib64.clone(),
            width: self.avd.lcd().0,
            height: self.avd.lcd().1,
            verbose: opts.verbose,
            display: display.clone(),
            vulkan_only: guest_on_angle(&self.avd),
            virtual_queue: virtual_queue(&guest_egl(&self.avd), AngleImage::of(&self.avd)),
            // Their GLES encoder's transport (see `pipe_gles`).
            pipe_gles: pipe_gles(&self.avd),
            metal_present: opts.metal_present,
        })
    }

    /// `launched`: when the process started, for `-verbose` launch timings.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn machine_config(&self, opts: &Opts, launched: std::time::Instant) -> Result<emu_vmm::MachineConfig> {
        let plan = self.plan(opts);
        if let Some(missing) = plan.missing().first() {
            bail!("missing {}", missing.display());
        }
        for d in &plan.disks {
            if !d.path.is_file() {
                bail!("missing disk image {}", d.path.display());
            }
        }
        let mut initrd = std::fs::read(&plan.ramdisk).with_context(|| format!("reading {}", plan.ramdisk.display()))?;
        if !plan.bootconfig.entries().is_empty() {
            initrd.extend_from_slice(&plan.bootconfig.encode());
        }
        let log = self.console_log.try_clone().context("console.log")?;
        let console: Box<dyn Write + Send> = if opts.show_kernel { Box::new(Tee(std::io::stdout(), log)) } else { Box::new(log) };
        let logcat = self.logcat.clone();

        Ok(emu_vmm::MachineConfig {
            cpus: plan.cpus,
            ram_bytes: plan.ram_bytes,
            kernel: plan.kernel,
            // Inflated gzip kernels, beside vkprobe's verdicts.
            kernel_cache: std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/simee")),
            initrd: Some(initrd),
            cmdline: plan.cmdline,
            disks: plan.disks,
            disk_fsync: opts.disk_fsync,
            rng: true,
            adb: Some(self.adb.clone()),
            gpu: self.gpu_spec(opts),
            boot_props: plan.boot_props,
            input: Some((self.avd.lcd().0, self.avd.lcd().1)),
            touch_screen: plan.touch_screen,
            touchpad: plan.touchpad,
            dual_mode_mouse: plan.dual_mode_mouse,
            xr: self.xr.clone(),
            sensors: self.sensors.clone(),
            clipboard: self.clipboard.clone(),
            fingerprint: self.fingerprint.clone(),
            gps: self.gps.clone(),
            battery: self.battery.clone(),
            telephony: self.telephony.clone(),
            network: self.network.clone(),
            car: self.car.clone(),
            sound: plan.sound.then_some(emu_vmm::audio::Options {
                output: !opts.no_audio,
                input: opts.allow_host_audio && !opts.no_audio,
                verbose: opts.verbose,
            }),
            on_guest_event: {
                let started = std::time::Instant::now();
                let verbose = opts.verbose;
                Some(std::sync::Arc::new(move |event: &str| {
                    if event == "bootcomplete" {
                        // On stdout, like the launch line: tools can watch for it.
                        println!("emu: boot completed in {:.1}s", started.elapsed().as_secs_f64());
                        // What the boot cost the vCPUs (scripts/android-bench.sh reads vcpu0's).
                        if let Some(exits) = emu_vmm::exit_counts().filter(|_| verbose) {
                            for line in crate::exit_lines(&exits) {
                                eprintln!("emu: at boot completed:{line}");
                            }
                        }
                    }
                }))
            },
            on_input: {
                let slot = self.inputs.clone();
                Some(std::sync::Arc::new(move |i| *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(i)))
            },
            console,
            logcat,
            launched: opts.verbose.then_some(launched),
        })
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl Instance {
    /// Software mode's config (#55): the plan on QEMU's TCG with emu's devices
    /// (`emu_vmm::soft`). The renderer runs on SwiftShader: the SDK's lavapipe crashed in a render
    /// thread on every booted guest (docs/android.md); `-gpu lavapipe` picks it anyway.
    pub fn soft_config(&self, opts: &Opts, qemu: PathBuf, accel: emu_vmm::soft::Accel) -> Result<emu_vmm::soft::Config> {
        let plan = self.plan(opts);
        if !plan.x86 {
            bail!(
                "{}: software mode runs x86_64 images on stock's x86 board; this AVD's image is arm64 (create one from an x86_64 image)",
                self.avd.name
            );
        }
        if let Some(missing) = plan.missing().first() {
            bail!("missing {}", missing.display());
        }
        let mut initrd = std::fs::read(&plan.ramdisk).with_context(|| format!("reading {}", plan.ramdisk.display()))?;
        if !plan.bootconfig.entries().is_empty() {
            initrd.extend_from_slice(&plan.bootconfig.encode());
        }
        let log = self.console_log.try_clone().context("console.log")?;
        let console: Box<dyn Write + Send> = if opts.show_kernel { Box::new(Tee(std::io::stdout(), log)) } else { Box::new(log) };
        let icd = if opts.gpu.as_deref() == Some("lavapipe") { emu_gfxstream::Icd::Lavapipe } else { emu_gfxstream::Icd::SwiftShader };
        let (w, h, _) = self.avd.lcd();
        let gpu = self.gpu_lib64.as_ref().zip(self.display.as_ref()).map(|(lib64, display)| emu_vmm::soft::Gpu {
            lib64: lib64.clone(),
            icd,
            width: w,
            height: h,
            display: display.clone(),
            verbose: opts.verbose,
        });
        let started = std::time::Instant::now();
        Ok(emu_vmm::soft::Config {
            qemu,
            // Short: unix socket paths are limited to 108 bytes.
            dir: std::env::temp_dir().join(format!("simee-{}", std::process::id())),
            cpus: plan.cpus,
            ram_bytes: plan.ram_bytes,
            kernel: plan.kernel,
            initrd,
            cmdline: plan.cmdline,
            disks: plan.disks,
            disk_fsync: opts.disk_fsync,
            adb: self.adb.clone(),
            gpu,
            boot_props: plan.boot_props,
            on_guest_event: Some(std::sync::Arc::new(move |event: &str| {
                if event == "bootcomplete" {
                    // On stdout, like the launch line: tools can watch for it.
                    println!("emu: boot completed in {:.1}s", started.elapsed().as_secs_f64());
                }
            })),
            sensors: self.sensors.clone(),
            clipboard: self.clipboard.clone(),
            fingerprint: self.fingerprint.clone(),
            gps: self.gps.clone(),
            telephony: self.telephony.clone(),
            network: self.network.clone(),
            car: self.car.clone(),
            xr: self.xr.clone(),
            input: Some((w, h)),
            touch_screen: plan.touch_screen,
            on_input: {
                let slot = self.inputs.clone();
                Some(std::sync::Arc::new(move |i| *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(i)))
            },
            console,
            logcat: self.logcat.clone(),
            verbose: opts.verbose,
            accel,
            accel_fallback: opts.accel.is_none(),
        })
    }
}

/// Console ports stock and simee pick from (adb is each one + 1); stock's ceiling.
#[cfg_attr(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))), allow(dead_code))]
pub const PORTS: std::ops::RangeInclusive<u16> = 5554..=5682;

/// Claims console port `port` for this process: its lock file, held until the returned file is
/// dropped or the process exits. None when another simee instance holds it.
#[cfg_attr(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))), allow(dead_code))]
pub fn claim_port(dir: &Path, port: u16) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let f = std::fs::File::create(dir.join(format!("{port}.lock"))).ok()?;
    f.try_lock().ok().map(|()| f)
}

/// The first even console port in `range` this process could claim whose console and adb
/// ports are free (`free`), with its claim. Instances launched together used to race between
/// probing a pair free and binding it (both picked the same pair, one then failed); the claim is
/// taken before the probe and kept, so two simee instances never pick one pair. A stock
/// emulator doesn't take claims, so the bind probe still keeps clear of the ones it holds.
#[cfg_attr(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))), allow(dead_code))]
pub fn pick_port(dir: &Path, range: std::ops::RangeInclusive<u16>, free: impl Fn(u16) -> bool) -> Option<(u16, std::fs::File)> {
    range.step_by(2).find_map(|p| {
        let claim = claim_port(dir, p)?;
        (free(p) && free(p + 1)).then_some((p, claim))
    })
}

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
fn port_free(p: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, p)).is_ok()
}

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
/// Creates `image` as a sparse, empty ext4 filesystem of `size` bytes.
fn create_userdata(image: &std::path::Path, size: u64) -> Result<()> {
    let mut uuid = [0u8; 16];
    sim_core::entropy::fill(&mut uuid).context("reading random bytes from the OS")?;
    uuid[6] = (uuid[6] & 0x0F) | 0x40; // RFC 4122 version 4 (random)
    uuid[8] = (uuid[8] & 0x3F) | 0x80;
    let file = File::options().read(true).write(true).create_new(true).open(image)?;
    emu_ext4::format(&file, size, "userdata", uuid).with_context(|| format!("formatting {} as ext4", image.display()))
}

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
struct Tee<A, B>(A, B);

#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
impl<A: Write, B: Write> Write for Tee<A, B> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(buf)?;
        self.1.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()?;
        self.1.flush()
    }
}

/// `-logcat-output FILE` rolls over to `FILE.1` past this size, so a long-lived device keeps at
/// most twice this on disk.
pub const LOGCAT_ROTATE_BYTES: u64 = 4 << 20;

/// Guest buffers logcat's spool holds while its writer is behind (each one hvc1 buffer, up to a
/// page); beyond that output is dropped and counted rather than stalling the console worker.
const LOGCAT_SPOOL_DEPTH: usize = 1024;

/// Where logcat's stream goes for this process: `-logcat-output`'s file (rotated), stdout with
/// `-logcat` alone, and `watch`, the display that turns as the guest's log says it rotated
/// ([`rotation_watch`]); nowhere without either. The user's output is behind a spool, which
/// drops output when its writer falls behind; the watch reads every chunk inline, before the
/// spool's queue, on the thread that hands it the guest's log (the console worker), so it never
/// misses a rotation line.
#[cfg_attr(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))), allow(dead_code))]
pub fn logcat_sink(
    opts: &Opts,
    watch: Option<std::sync::Arc<emu_vmm::display::FrameStore>>,
) -> std::io::Result<Option<emu_vmm::virtio::console::Spool>> {
    use emu_vmm::virtio::console::Spool;
    let out: Option<Box<dyn std::io::Write + Send>> = match (&opts.logcat_output, &opts.logcat) {
        (Some(path), _) => Some(Box::new(RotatingFile::create(Path::new(path), LOGCAT_ROTATE_BYTES)?)),
        (None, Some(_)) => Some(Box::new(std::io::stdout())),
        (None, None) => None,
    };
    Ok(match (out, watch) {
        (None, None) => None,
        (None, Some(display)) => Some(Spool::tap_only(rotation_watch(display))),
        // Nothing but the user's: as they asked, untouched.
        (Some(out), None) => Some(Spool::new("logcat-out", out, LOGCAT_SPOOL_DEPTH)?),
        // The guest also logs the tags emu reads for itself; the user's output drops them.
        (Some(out), Some(display)) => {
            let filter = sim_core::logcat::Filter::parse(user_logcat_filter(opts));
            let out = Box::new(UserLines { out, filter, lines: Lines::default() });
            Some(Spool::new("logcat-out", out, LOGCAT_SPOOL_DEPTH)?.with_tap(rotation_watch(display)))
        }
    })
}

/// Reads the guest's logcat stream a line at a time: a line that reports the display's rotation
/// ([`sim_core::rotation::guest_rotation`]) turns `display`'s frames and touches to match. Cheap
/// enough to run inline: a line is copied once and its tag compared.
fn rotation_watch(display: std::sync::Arc<emu_vmm::display::FrameStore>) -> emu_vmm::virtio::console::Tap {
    let mut lines = Lines::default();
    Box::new(move |buf| {
        lines
            .feed(buf, |line| {
                if let Some(r) = sim_core::rotation::guest_rotation(&String::from_utf8_lossy(line)) {
                    display.set_rotation(r);
                }
                Ok(())
            })
            .unwrap_or(())
    })
}

/// The lines of the guest's log the user's filter shows, passed on to `out` (the tags emu added
/// for itself aren't, unless the user's filter shows them too).
struct UserLines {
    out: Box<dyn std::io::Write + Send>,
    filter: sim_core::logcat::Filter,
    lines: Lines,
}

impl std::io::Write for UserLines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (out, filter) = (&mut self.out, &self.filter);
        self.lines.feed(buf, |line| if filter.passes(String::from_utf8_lossy(line).trim_end()) { out.write_all(line) } else { Ok(()) })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// A byte stream cut into lines.
#[derive(Default)]
struct Lines(Vec<u8>);

/// A line longer than this is taken as it is (logcat's own limit is about 4 KB a message).
const LOGCAT_LINE_MAX: usize = 16 << 10;

impl Lines {
    /// Hands each line `buf` completes to `line`, with its end. A line is done with even when
    /// `line` fails (a full disk loses it, nothing more); the first failure is returned.
    fn feed(&mut self, buf: &[u8], mut line: impl FnMut(&[u8]) -> std::io::Result<()>) -> std::io::Result<()> {
        for piece in buf.split_inclusive(|b| *b == b'\n') {
            self.0.extend_from_slice(piece);
            if piece.ends_with(b"\n") || self.0.len() >= LOGCAT_LINE_MAX {
                let done = line(&self.0);
                self.0.clear();
                done?;
            }
        }
        Ok(())
    }
}

/// A log file that, before a write would take it past `cap` bytes, is renamed to `<path>.1`
/// (replacing the previous one) and started again.
pub struct RotatingFile {
    path: PathBuf,
    file: std::fs::File,
    len: u64,
    cap: u64,
}

impl RotatingFile {
    /// Creates (truncates) `path`.
    pub fn create(path: &Path, cap: u64) -> std::io::Result<RotatingFile> {
        let file = std::fs::File::create(path)?;
        Ok(RotatingFile { path: path.to_owned(), file, len: 0, cap })
    }

    fn rotated(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".1");
        PathBuf::from(p)
    }
}

impl std::io::Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.len > 0 && self.len + buf.len() as u64 > self.cap {
            std::io::Write::flush(&mut self.file)?;
            std::fs::rename(&self.path, self.rotated())?;
            self.file = std::fs::File::create(&self.path)?;
            self.len = 0;
        }
        std::io::Write::write_all(&mut self.file, buf)?;
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.file)
    }
}

fn gpu_off(opts: &Opts) -> bool {
    matches!(opts.gpu.as_deref(), Some("off" | "none"))
}

/// The gfxstream renderer's `lib64` the AVD's SDK offers, unless `-gpu off`.
pub fn renderer(avd: &Avd, opts: &Opts) -> Option<PathBuf> {
    if gpu_off(opts) { None } else { avd.sdk_root.as_deref().and_then(emu_sdk::renderer::find_lib64) }
}

fn is_xr(avd: &Avd) -> bool {
    avd.profile().kind.sub_platform == SubPlatform::Android(AndroidOs::Xr)
}

/// Everything a boot of one AVD involves, decided from its config and image with no side
/// effects: what the guest is told (kernel command line, bootconfig, boot properties) and what
/// the board gives it (disks, devices, sensors).
pub struct BootPlan {
    pub avd: String,
    /// `Android TV · TV`, ...
    pub kind: String,
    pub kernel: PathBuf,
    pub ramdisk: PathBuf,
    pub cmdline: String,
    pub bootconfig: BootConfig,
    pub boot_props: Vec<(String, String)>,
    pub disks: Vec<DiskSpec>,
    pub cpus: u32,
    pub ram_bytes: u64,
    /// Width, height, density.
    pub screen: (u32, u32, u32),
    /// The gfxstream renderer's `lib64`; None boots without graphics.
    pub renderer: Option<PathBuf>,
    /// The virtio-snd card (ALSA card 0).
    pub sound: bool,
    /// `qemud:xr_service`.
    pub xr: bool,
    /// The vehicle HAL channel, `qemud:car`.
    pub car: bool,
    /// Sensor handles the guest's sensors HAL is offered.
    pub sensor_mask: u32,
    /// A multi-touch screen (not on `hw.screen=no-touch`, TV's).
    pub touch_screen: bool,
    /// A touchpad's size in units (`hw.touchpad0`, the display glasses').
    pub touchpad: Option<(u32, u32)>,
    /// The dual-mode mouse: XR images that enable `VirtioDualModeMouse` (the headsets, not the
    /// display glasses), as stock adds it.
    pub dual_mode_mouse: bool,
    /// The GLES encoder goes over the goldfish `opengles` pipe and the board has no virtio-gpu
    /// (API 32 and older, see [`pipe_gles`]).
    pub pipe_gles: bool,
    /// An x86_64 image: stock's x86 board under QEMU's TCG (software mode, #55).
    pub x86: bool,
}

impl BootPlan {
    pub fn new(avd: &Avd, opts: &Opts, renderer: Option<PathBuf>) -> BootPlan {
        let dir = emu_avd::state_dir(&avd.dir);
        let user_mode = if opts.read_only { DiskMode::Ephemeral } else { DiskMode::Persistent };
        let disk = |slot, path: PathBuf, mode, serial: &str| DiskSpec { slot, path, mode, serial: serial.to_string() };
        let mut cmdline = String::from(if avd.is_x86_64() { X86_CMDLINE } else { BASE_CMDLINE });
        if !full_console(opts) {
            cmdline.push(' ');
            cmdline.push_str(QUIET_CONSOLE);
        }
        let extra = avd.image_cmdline();
        if !extra.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(&extra);
        }
        let (xr, car) = (is_xr(avd), avd.is_car());
        let mut plan = BootPlan {
            avd: avd.name.clone(),
            kind: avd.profile().kind.label(),
            kernel: avd.kernel(),
            ramdisk: avd.ramdisk(),
            cmdline,
            bootconfig: BootConfig::new(),
            boot_props: boot_props(avd),
            disks: vec![
                disk(slots::SYSTEM, avd.system_image(), DiskMode::Ephemeral, "system"),
                disk(slots::CACHE, dir.join("cache.img"), user_mode, "cache"),
                disk(slots::USERDATA, dir.join("userdata.img"), user_mode, "userdata"),
                disk(slots::METADATA, dir.join("encryptionkey.img"), user_mode, "metadata"),
                disk(slots::VENDOR, avd.vendor_image(), DiskMode::Ephemeral, "vendor"),
            ],
            cpus: opts.cores.unwrap_or_else(|| avd.cpus()),
            ram_bytes: opts.memory_mb.map_or_else(|| avd.ram_bytes(), |mb| mb << 20),
            screen: avd.lcd(),
            renderer,
            sound: sound(xr, car, avd.image_feature("VirtioSndCard").unwrap_or(false)),
            xr,
            car,
            sensor_mask: avd.sensor_mask(),
            touch_screen: avd.touch_screen(),
            touchpad: avd.touchpad(),
            dual_mode_mouse: xr && avd.image_feature("VirtioDualModeMouse").unwrap_or(true),
            pipe_gles: pipe_gles(avd),
            x86: avd.is_x86_64(),
        };
        let bc = bootconfig(avd, opts, plan.renderer.is_some());
        if takes_bootconfig(avd) {
            plan.bootconfig = bc;
        } else {
            // API 30 and older: kernels without bootconfig (4.14, android11-5.4) take a bootconfig
            // block after the ramdisk as junk, fall back to an initrd and panic ("Unable to mount
            // root fs on unknown-block(0,0)"). Stock passes these images everything on the
            // command line instead, under the older names.
            plan.cmdline = plan.cmdline.replace(" bootconfig", "");
            plan.cmdline.push(' ');
            plan.cmdline.push_str(&legacy_cmdline(&bc, plan.renderer.is_some()));
            let filter = logcat_filter(avd, opts);
            let line = logcat_pipe_cmdline(&filter);
            if !line.contains(&filter.split_whitespace().collect::<Vec<_>>().join(",")) {
                eprintln!("emu: -logcat '{filter}' is too long for these images' qemu.cmdline property; logcat shows every tag (*:V)");
            }
            // First, then `qemu.logcat=start`, as stock sends them: qemu-props sets the second
            // as `vendor.qemu.logcat`, which starts `goldfish-logcat`, and `logcat -Q` reads
            // the first once and exits quietly if it isn't set yet.
            plan.boot_props.insert(0, ("qemu.cmdline".to_string(), line));
            plan.boot_props.insert(1, ("qemu.logcat".to_string(), "start".to_string()));
        }
        plan
    }

    /// Image files the boot reads that aren't there. The AVD's own disks under `simee/` are
    /// created at boot, so only the image's are checked.
    pub fn missing(&self) -> Vec<&Path> {
        let image = self.disks.iter().filter(|d| d.mode == DiskMode::Ephemeral && !d.path.starts_with(self.state_dir()));
        [self.kernel.as_path(), self.ramdisk.as_path()]
            .into_iter()
            .chain(image.map(|d| d.path.as_path()))
            .filter(|p| !p.is_file())
            .collect()
    }

    fn state_dir(&self) -> PathBuf {
        self.disks.iter().find(|d| d.serial == "userdata").and_then(|d| d.path.parent()).map(Path::to_path_buf).unwrap_or_default()
    }

    /// The virtio devices and host services the board gives the guest.
    pub fn devices(&self) -> Vec<&'static str> {
        let mut out = vec![
            "virtio-blk x5",
            "virtio-console (logcat, modem)",
            "virtio-net (link only)",
            "virtio-rng",
            "virtio-vsock (adb, qemud pipes)",
        ];
        out.push(if self.touch_screen {
            "virtio-input: multi-touch, keyboard (qwerty2), rotary"
        } else {
            "virtio-input: keyboard (qwerty2), rotary"
        });
        if self.dual_mode_mouse {
            out.push("virtio-input: dual-mode mouse");
        }
        if self.touchpad.is_some() {
            out.push("virtio-input: touchpad");
        }
        match (self.renderer.is_some(), self.pipe_gles) {
            (true, false) => out.push("virtio-gpu (gfxstream), goldfish address space"),
            (true, true) => out.push("goldfish pipe (opengles, GLProcessPipe; gfxstream), goldfish address space"),
            (false, _) => {}
        }
        if self.sound {
            out.push("virtio-snd");
        }
        out.push(if self.x86 { "QEMU's pc board (8250 UART, RTC), CPU in TCG" } else { "goldfish battery, PL011, PL031" });
        out.extend(["qemud:sensors", "qemud:gps", "qemud:fingerprintlisten", "qemud:camera", "qemud:boot-properties", "clipboard"]);
        if self.xr {
            out.push("qemud:xr_service");
        }
        if self.car {
            out.push("qemud:car (vehicle HAL)");
        }
        out
    }
}

impl std::fmt::Display for BootPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h, dpi) = self.screen;
        writeln!(f, "avd       {} ({})", self.avd, self.kind)?;
        writeln!(f, "machine   {} vCPUs, {} MB RAM, {w}x{h} @ {dpi} dpi", self.cpus, self.ram_bytes >> 20)?;
        writeln!(f, "kernel    {}", self.kernel.display())?;
        writeln!(f, "ramdisk   {} + bootconfig ({} entries)", self.ramdisk.display(), self.bootconfig.entries().len())?;
        writeln!(f, "cmdline   {}", self.cmdline)?;
        match &self.renderer {
            Some(lib64) => writeln!(f, "graphics  gfxstream from {}", lib64.display())?,
            None => writeln!(f, "graphics  none: no renderer (-gpu off, or none installed: emu -provision-renderer)")?,
        }
        writeln!(f, "devices   {}", self.devices().join(", "))?;
        let sensors: Vec<&str> =
            emu_vmm::pipe::sensors::NAMES.iter().enumerate().filter(|(h, _)| self.sensor_mask & (1 << h) != 0).map(|(_, n)| *n).collect();
        writeln!(f, "sensors   {}", sensors.join(", "))?;
        writeln!(f, "disks")?;
        for d in &self.disks {
            let mode = match d.mode {
                DiskMode::Persistent => "",
                DiskMode::Ephemeral => " (writes discarded)",
            };
            match emu_vmm::layout::x86::disk_slot(&d.serial).filter(|_| self.x86) {
                Some(pci) => writeln!(f, "  {:<9} pci 00:{pci:02x}.0 {}{mode}", d.serial, d.path.display())?,
                None => writeln!(f, "  {:<9} slot {:<2} {}{mode}", d.serial, d.slot, d.path.display())?,
            }
        }
        writeln!(f, "bootconfig")?;
        for (k, v) in self.bootconfig.entries() {
            writeln!(f, "  {k}={v}")?;
        }
        writeln!(f, "boot props")?;
        for (k, v) in &self.boot_props {
            writeln!(f, "  {k}={v}")?;
        }
        Ok(())
    }
}

/// The virtio-snd card, as stock adds it: for images that enable `VirtioSndCard` (phone, Wear,
/// TV, XR from API 34; their ranchu audio HAL plays through ALSA card 0), and always for
/// Automotive (`audio.primary.caremu` crashes without card 0) and XR. `EMU_VIRTIO_SND=1` or `0`
/// forces it on or off for any image. `-no-audio` keeps the card (as stock does) but silent.
fn sound(xr: bool, car: bool, feature: bool) -> bool {
    match std::env::var("EMU_VIRTIO_SND").as_deref() {
        Ok("1" | "on" | "yes") => true,
        Ok("0" | "off" | "no") => false,
        _ => car || xr || feature,
    }
}

fn bootconfig(avd: &Avd, opts: &Opts, graphics: bool) -> BootConfig {
    let (w, h, _density) = avd.lcd();
    let mut bc = BootConfig::new();
    bc.set("androidboot.hardware", "ranchu")
        .set("androidboot.boot_devices", if avd.is_x86_64() { emu_vmm::layout::x86::BOOT_DEVICES } else { "a003600.virtio_mmio" })
        .set("androidboot.qemu", "1")
        .set("androidboot.qemu.avd_name", avd.name.as_str())
        .set("androidboot.serialno", "EMULATOR0EMURS0")
        .set("androidboot.veritymode", "enforcing")
        .set("androidboot.dalvik.vm.heapsize", "576m")
        .set("androidboot.logcat", logcat_filter(avd, opts))
        .set("androidboot.qemu.skin", format!("{w}x{h}"))
        .set("androidboot.qemu.vsync", avd.vsync_hz().to_string())
        .set("androidboot.qemu.settings.system.screen_off_timeout", "2147483647")
        .set("androidboot.opengles.version", "196609");
    if graphics {
        if pipe_gles(avd) {
            // The GLES encoder over the goldfish pipe, no virtio-gpu (see `pipe_gles`), with
            // stock's bootconfig for it. Images without bootconfig get it as `qemu.gltransport`.
            bc.set("androidboot.hardware.gltransport", "pipe");
            if takes_bootconfig(avd) {
                bc.set("androidboot.qemu.gltransport.name", "pipe").set("androidboot.qemu.gltransport.drawFlushInterval", "800");
            }
        } else {
            // gfxstream over virtio-gpu: GLES through the pipe stream, Vulkan and ANGLE through
            // address-space graphics, both on the host renderer.
            bc.set("androidboot.hardware.gltransport", "virtio-gpu-asg");
        }
        bc.set("androidboot.hardware.vulkan", "ranchu");
        // API 34+: guest GLES through ANGLE on the guest's Vulkan, so every context uses
        // address-space graphics: with `emulation` GLES rides the virtio-gpu pipe stream,
        // whose blocking reads serialize all processes on one queue (API 34+ boots took
        // 14–28 s with ANRs, against 5–8 s on ANGLE). Below API 34 the images' ANGLE aborts
        // SurfaceFlinger, so they get the GLES encoder (`emulation`), as stock runs them; their
        // boot times aren't measured yet. EMU_GUEST_EGL overrides either default.
        for (k, v) in guest_graphics(&guest_egl(avd), AngleImage::of(avd)) {
            bc.set(k, v);
        }
    }
    if opts.no_boot_anim {
        bc.set("androidboot.debug.sf.nobootanimation", "1");
    }
    // A round screen (boot.prop's `ro.emulator.circular`), as stock tells the guest.
    if avd.circular() {
        bc.set("androidboot.emulator.circular", "1");
    }
    // A Pixel Fold image's device states for its hinge, as stock sends them.
    if let Some(states) = crate::fold::guest_device_states(avd) {
        bc.set("androidboot.qemu.device_state", states);
    }
    for (k, v) in avd.vbmeta_params() {
        bc.set(k, v);
    }
    // Pre-authorize the host's adb key, like the stock emulator.
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(key) = std::fs::read_to_string(PathBuf::from(home).join(".android/adbkey.pub"))
    {
        bc.set("androidboot.qemu.adb.pubkey", key.trim());
    }
    bc
}

/// Whether the image's kernel reads bootconfig: images say so with `AndroidbootProps2` (every
/// image from API 31 on), and stock passes bootconfig only then. An image that says nothing and
/// isn't known to be older (no `advancedFeatures.ini`, or no API level) is taken as new.
fn takes_bootconfig(avd: &Avd) -> bool {
    avd.image_feature("AndroidbootProps2") == Some(true) || avd.api_level().is_none_or(|api| api >= 31)
}

/// Whether the guest's GLES encoder goes over the goldfish `opengles` pipe and the board has no
/// virtio-gpu, as stock runs every image (`gltransport=pipe`): images without bootconfig (API 30
/// and older, whose kernels oops on virtio-gpu), and API 31 and 32. On those two the gralloc
/// mapper inside `media.swcodec` opens the transport for its CPU reads of color buffers, and
/// their SELinux policy denies `mediaswcodec` the render node (`gpu_device`) while it allows the
/// goldfish pipe (`qemu_device`): over virtio-gpu, `screenrecord`'s encoder crashed (#64). API 33
/// and newer allow the render node and keep virtio-gpu.
fn pipe_gles(avd: &Avd) -> bool {
    !takes_bootconfig(avd) || avd.api_level().is_some_and(|api| (31..=32).contains(&api))
}

/// The host ANGLE backend for a boot that shouldn't use Metal, if any; `ANGLE_DEFAULT_PLATFORM`
/// overrides it either way. SwiftShader:
/// - for an API 29 image's first boot. There a guest `glClear` reaches ANGLE's Metal clear with
///   a color attachment whose texture is gone and faults; gfxstream's exception handler retries
///   the load forever under ANGLE's global lock, and every render thread stops behind it (#94).
///   Stock renders a headless first boot with ANGLE on SwiftShader too. Later boots (user data
///   exists) and API 30 run on Metal.
/// - when the renderer runs without GL (`without_gl`, [`emu_vmm::runs_without_gl`]): nothing
///   draws with the host's GLES then, but gfxstream still opens ANGLE's display at init and makes
///   one context, and on Metal that context waits for ANGLE's built-in shader library to compile:
///   about 4.3 s whenever Metal's shader cache misses (a new emu path, a macOS update, an evicted
///   cache), and the boot waits for the init (#70). On SwiftShader the display takes about 30 ms
///   and compiles nothing.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn host_angle(api: Option<u32>, first_boot: bool, without_gl: bool) -> Option<&'static str> {
    (without_gl || (first_boot && api == Some(29))).then_some("swiftshader")
}

/// What `-verbose` says when a boot runs on KosmicKrisp ([`host_gpu_icd`]).
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
const KOSMICKRISP_NOTICE: &str = "emu: API 35 draws its UI with Skia GL on the guest's ANGLE: host Vulkan is KosmicKrisp, \
     which compiles the pipelines MoltenVK refuses (#412; ANDROID_EMU_VK_ICD=moltenvk overrides)";

/// The host-GPU Vulkan driver for `avd` with guest GLES `egl`: KosmicKrisp when the guest's UI
/// draws with Skia GL on its own ANGLE on API 35 (phones, Automotive 35) and the renderer ships
/// it (`kosmickrisp`), else MoltenVK; `asked` (`ANDROID_EMU_VK_ICD`) picks either by name. That
/// ANGLE asks for vertex inputs whose numeric type differs from the shader's (a float format for
/// a `uvec2` input), which Vulkan allows and Metal refuses: under MoltenVK each such pipeline
/// fails to compile (15,303 `Render pipeline compile failed` on one API 34 boot) and the draw is
/// lost. KosmicKrisp compiles them. API 34 has the same stack, but on KosmicKrisp its screen
/// stays blank (guest `screencap` all white, host frame all black, #495), while MoltenVK still
/// draws most of it, so API 34 (phones, TV, Automotive, google-xr) stays on MoltenVK;
/// `ANDROID_EMU_VK_ICD=kosmickrisp` runs it on KosmicKrisp. API 36 and newer draw their UI with
/// Skia on Vulkan and API 30 to 33 with the GLES encoder; they stay on MoltenVK. The selection
/// is by API level as well as by the missing `VulkanVirtualQueue` tag: an API 36 image without
/// it (Wear OS 36) stays on MoltenVK until a device check shows KosmicKrisp draws it. See
/// docs/research/api34-moltenvk.md.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn host_gpu_icd(avd: &Avd, egl: &str, kosmickrisp: bool, asked: Option<&str>) -> emu_gfxstream::Icd {
    let api_35 = avd.api_level() == Some(35);
    let ui_gl_on_angle = api_35 && egl == "angle" && takes_bootconfig(avd) && !AngleImage::of(avd).virtual_queue;
    emu_gfxstream::gpu_icd(ui_gl_on_angle, kosmickrisp, asked)
}

/// The driver a host-GPU boot runs: `picked` ([`host_gpu_icd`]), except that KosmicKrisp needs
/// its probe (`kosmickrisp`, run only then) to pass; otherwise MoltenVK, with the notice saying
/// why. An unclear probe (a timeout, a crash) isn't cached, so the next boot probes again.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn checked_host_icd(
    picked: emu_gfxstream::Icd,
    kosmickrisp: impl FnOnce() -> crate::vkprobe::Verdict,
) -> (emu_gfxstream::Icd, Option<String>) {
    use crate::vkprobe::Verdict;
    use emu_gfxstream::Icd;
    if picked != Icd::KosmicKrisp {
        return (picked, None);
    }
    match kosmickrisp() {
        Verdict::Pass => (Icd::KosmicKrisp, None),
        Verdict::Fail => (Icd::MoltenVk, Some("emu: KosmicKrisp didn't come up here; using MoltenVK".into())),
        Verdict::Unclear(why) => {
            (Icd::MoltenVk, Some(format!("emu: KosmicKrisp's probe {why}; using MoltenVK for this boot (the next boot probes again)")))
        }
    }
}

/// The guest draws with ANGLE on its Vulkan (an image that boots on bootconfig, its GLES ANGLE),
/// so the renderer can run without GL ([`emu_vmm::GpuSpec::vulkan_only`]). Images without
/// bootconfig have no ANGLE: their GLES is the encoder, on the host's GL.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn guest_on_angle(avd: &Avd) -> bool {
    guest_egl(avd) == "angle" && takes_bootconfig(avd)
}

/// Why emu can't run this AVD's image yet, if it can't: images below API 29, which emu has never
/// been brought up on. emu refuses them up front with the reason; `EMU_ALLOW_OLD_IMAGES=1` boots
/// them anyway, to work on them. (API 29–32 guests draw through the goldfish `opengles` pipe,
/// `emu_vmm::pipe::opengles`; API 33 guests set up address-space graphics the old way, which
/// the GPU translates, `virtio/legacy_asg.rs`.)
pub fn unsupported(avd: &Avd) -> Option<String> {
    let api = avd.api_level()?;
    let why = match api {
        ..=28 => "emu was brought up on API 29 and newer, and older boards differ (goldfish devices where newer ones have virtio)",
        _ => return None,
    };
    Some(format!(
        "{}: API {api} images don't run on emu yet: {why}. emu runs API 29 and newer; create the AVD from a newer image (`emu -provision 36`), or boot this one with the SDK's emulator",
        avd.name
    ))
}

/// Whether `EMU_ALLOW_OLD_IMAGES` lets an unsupported image boot anyway.
pub fn allow_old_images() -> bool {
    std::env::var("EMU_ALLOW_OLD_IMAGES").is_ok_and(|v| v == "1")
}

/// The kernel command line stock gives images without `AndroidbootProps2` (API 30 and older,
/// whose kernels lack bootconfig), from the same entries: `androidboot.qemu.*` as `qemu.*`, the
/// few properties bootconfig renamed back to their `qemu.*` names, the rest as they are. Init
/// splits the command line on spaces without quoting, so a value with a space (the adb key) is
/// left out; those images' adbd doesn't read it. ANGLE isn't on these images: GLES is the
/// image's own `emulation` driver (`qemu.gles=1`).
fn legacy_cmdline(bc: &BootConfig, graphics: bool) -> String {
    let mut out = vec!["android.qemud=1".to_string(), "qemu.encrypt=1".to_string()];
    if graphics {
        out.push("qemu.gles=1".to_string());
    }
    for (k, v) in bc.entries() {
        if v.contains(char::is_whitespace) || k == "androidboot.hardwareegl" || ANGLE_UI_KEYS.contains(&k.as_str()) {
            continue;
        }
        let key = match k.as_str() {
            "androidboot.qemu" => "qemu".to_string(),
            "androidboot.dalvik.vm.heapsize" => "qemu.dalvik.vm.heapsize".to_string(),
            "androidboot.opengles.version" => "qemu.opengles.version".to_string(),
            // These images' GLES encoder has no address-space graphics: stock gives them the
            // goldfish pipe.
            "androidboot.hardware.gltransport" => {
                out.push("qemu.gltransport=pipe".to_string());
                continue;
            }
            _ => match k.strip_prefix("androidboot.qemu.") {
                Some(rest) => format!("qemu.{rest}"),
                None => k.clone(),
            },
        };
        out.push(format!("{key}={v}"));
    }
    out.join(" ")
}

/// `qemu.cmdline` for images without a logcat console (API 30 and older): their
/// `goldfish-logcat` runs `logcat -Q`, which takes its filter and destination from this property
/// (SELinux keeps it from `/proc/cmdline`), as stock sets it: `androidboot.logcat=<filter>` and
/// `androidboot.consolepipe=qemu_pipe,pipe:logcat`. logcat stops the filter at the first space
/// and splits it on commas, so the filter's spaces become commas. The property isn't read-only,
/// so its value is at most [`PROP_VALUE_MAX`] bytes and a longer one isn't set at all (logcat -Q
/// then exits quietly): a filter that doesn't fit becomes `*:V`, every tag, which does.
fn logcat_pipe_cmdline(filter: &str) -> String {
    let line = |f: &str| format!("androidboot.logcat={f} androidboot.consolepipe=qemu_pipe,pipe:logcat");
    let full = line(&filter.split_whitespace().collect::<Vec<_>>().join(","));
    if full.len() <= PROP_VALUE_MAX { full } else { line("*:V") }
}

/// The longest value bionic stores for a property that isn't `ro.` (`PROP_VALUE_MAX` less the
/// NUL).
const PROP_VALUE_MAX: usize = 91;

/// The guest's GLES driver: `EMU_GUEST_EGL` if set, else [`default_egl`]. Under ANGLE no guest
/// process uses the host's GL, so the renderer runs without it
/// ([`emu_vmm::GpuSpec::vulkan_only`]).
fn guest_egl(avd: &Avd) -> String {
    std::env::var("EMU_GUEST_EGL").unwrap_or_else(|_| default_egl(avd.api_level()).into())
}

/// ANGLE on the guest's Vulkan, except on images below API 34: their ANGLE finds no EGL config
/// for SurfaceFlinger on the renderer's Vulkan (API 33 aborts "no suitable EGLConfig found" and
/// restarts forever), so they get the GLES encoder (`emulation`) and a renderer with GL, as stock
/// runs them.
fn default_egl(api: Option<u32>) -> &'static str {
    if api.is_some_and(|api| api < 34) { "emulation" } else { "angle" }
}

/// What an image's `advancedFeatures.ini` says about drawing its UI when its GLES is ANGLE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AngleImage {
    /// `VulkanVirtualQueue` (the API 36 and newer images): its Vulkan driver takes a graphics
    /// queue family with two queues, which HWUI on Vulkan asserts, and the image maps
    /// RenderEngine's backend from bootconfig (API 35 hard-codes `skiaglthreaded`).
    virtual_queue: bool,
    /// `GuestAngle` (google-xr 34): the image asks for ANGLE itself.
    guest_angle: bool,
}

impl AngleImage {
    fn of(avd: &Avd) -> Self {
        AngleImage {
            virtual_queue: avd.image_feature("VulkanVirtualQueue") == Some(true),
            guest_angle: avd.image_feature("GuestAngle") == Some(true),
        }
    }
}

/// The bootconfig keys [`guest_graphics`] adds for ANGLE guests: what the UI draws with and the
/// ANGLE features it hides. Images without bootconfig (the legacy command line) have no ANGLE.
const ANGLE_UI_KEYS: [&str; 4] = [
    "androidboot.debug.hwui.renderer",
    "androidboot.debug.renderengine.backend",
    "androidboot.hardware.angle_feature_overrides_disabled",
    "androidboot.hardware.angle_feature_overrides_enabled",
];

/// The bootconfig entries for a guest GLES driver `egl` on `image`. Under ANGLE they are what
/// stock 37.2 gives a `GuestAngle` guest on such an image (its `/proc/bootconfig` with
/// `-feature GuestAngle` on API 36, and on google-xr 34, which enables it):
/// - with `VulkanVirtualQueue`, the UI toolkit (HWUI) and SurfaceFlinger draw with Skia on the
///   guest's Vulkan (`skiavk`, `skiavkthreaded`), not Skia GL on ANGLE on that Vulkan: Skia GL
///   on ANGLE on MoltenVK leaves some apps' buffers fully transparent (Gboard's keyboard was a
///   blank panel; docs/android.md, "Gboard's blank keyboard");
/// - with `GuestAngle` alone, HWUI stays Skia GL (`skiagl`) and RenderEngine is the image's;
/// - with either, ANGLE hides its non-conformant `exposeN*` extensions.
///
/// Images with neither (API 34 and 35 phones, Automotive) keep their own choice: emu runs them
/// on ANGLE where stock wouldn't, and stock gives them no `GuestAngle` keys to copy. The GLES
/// encoder (`EMU_GUEST_EGL=emulation`) gets none, as stock without `GuestAngle`.
fn guest_graphics(egl: &str, image: AngleImage) -> Vec<(&'static str, String)> {
    let mut out = vec![("androidboot.hardwareegl", egl.to_string())];
    if egl != "angle" {
        return out;
    }
    let [hwui, renderengine, overrides_disabled, overrides_enabled] = ANGLE_UI_KEYS;
    if image.virtual_queue {
        out.extend([(hwui, "skiavk".to_string()), (renderengine, "skiavkthreaded".to_string())]);
    } else if image.guest_angle {
        out.push((hwui, "skiagl".to_string()));
    } else {
        return out;
    }
    out.extend([(overrides_disabled, "exposeN*".to_string()), (overrides_enabled, String::new())]);
    out
}

/// Whether the renderer offers a graphics queue family with more than one queue
/// (`VulkanVirtualQueue`, [`emu_vmm::GpuSpec::virtual_queue`]), which HWUI on Vulkan needs: only
/// when the guest is on ANGLE (so [`guest_graphics`] gives it `skiavk`) and its image
/// advertises the feature.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
fn virtual_queue(egl: &str, image: AngleImage) -> bool {
    egl == "angle" && image.virtual_queue
}

/// What the stock host hands `qemu-props` at boot.
fn boot_props(avd: &Avd) -> Vec<(String, String)> {
    let mut props = avd.qemu_props();
    if let Ok(tz) = std::fs::read_link("/etc/localtime")
        && let Some(name) = tz.to_str().and_then(|s| s.split("zoneinfo/").nth(1))
    {
        props.push(("qemu.timezone".to_string(), name.to_string()));
    }
    props
}

#[cfg(test)]
mod tests {
    use emu_avd::{Env, NewAvd};

    use super::*;

    /// An SDK with one image per form factor (its tag, `build.prop` and `advancedFeatures.ini`
    /// as the real images have them) and an AVD made from each the way `emu -create-avd` does.
    struct Sdk {
        root: PathBuf,
        env: Env,
    }

    impl Drop for Sdk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn sdk(tag: &str) -> Sdk {
        let root = std::env::temp_dir().join(format!("emu-plan-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk");
        let images: [(&str, &str, &str); 7] = [
            ("android-36/google_apis/arm64-v8a", "ro.product.name=sdk_gphone64_arm64\n", "VirtioSndCard = on\nVulkanVirtualQueue = on\n"),
            ("android-36/android-wear-signed/arm64-v8a", "", "VirtioSndCard = on\n"),
            ("android-34/android-tv/arm64-v8a", "", "VirtioSndCard = on\n"),
            ("android-34-ext9/android-automotive/arm64-v8a", "ro.product.name=sdk_car_arm64\n", "VirtioSndCard = on\n"),
            ("android-34/google-xr/arm64-v8a", "", "GuestAngle = on\nVirtioDualModeMouse = on\n"),
            ("android-36/ai-glasses/arm64-v8a", "", "VirtioSndCard = on\nXrStatusLed = on\nVulkanVirtualQueue = on\n"),
            ("android-36/default/x86_64", "ro.product.name=sdk_phone64_x86_64\n", "VirtioSndCard = on\nVulkanVirtualQueue = on\n"),
        ];
        for (dir, build_prop, features) in images {
            let d = sdk.join("system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            for f in ["kernel-ranchu", "ramdisk.img", "system.img", "vendor.img"] {
                std::fs::write(d.join(f), b"x").expect("image file");
            }
            if !build_prop.is_empty() {
                std::fs::write(d.join("build.prop"), build_prop).expect("build.prop");
            }
            std::fs::write(d.join("advancedFeatures.ini"), features).expect("features");
            std::fs::write(d.join("VerifiedBootParams.textproto"), "param: \"androidboot.vbmeta.size=6592\"\n").expect("vbmeta");
        }
        std::fs::write(sdk.join("system-images/android-36/ai-glasses/arm64-v8a/kernel_cmdline.txt"), "8250.nr_uarts=1\n").expect("cmdline");
        let env = Env { sdk_root: Some(sdk.clone()), avd_home: root.join("avd") };
        for (name, dir) in [
            ("phone", "android-36/google_apis/arm64-v8a"),
            ("watch", "android-36/android-wear-signed/arm64-v8a"),
            ("tv", "android-34/android-tv/arm64-v8a"),
            ("car", "android-34-ext9/android-automotive/arm64-v8a"),
            ("headset", "android-34/google-xr/arm64-v8a"),
            ("glasses", "android-36/ai-glasses/arm64-v8a"),
            ("x86", "android-36/default/x86_64"),
        ] {
            env.create_avd(&NewAvd::for_image(name, &format!("system-images/{dir}"))).expect("create");
        }
        Sdk { root, env }
    }

    /// Adds an image of `api` (with `features` as its `advancedFeatures.ini`) and an AVD on it.
    fn add_image(s: &Sdk, name: &str, api: u32, features: &str) {
        let rel = format!("system-images/android-{api}/default/arm64-v8a");
        let d = s.root.join("sdk").join(&rel);
        std::fs::create_dir_all(&d).expect("image dir");
        for f in ["kernel-ranchu", "ramdisk.img", "system.img", "vendor.img"] {
            std::fs::write(d.join(f), b"x").expect("image file");
        }
        std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel={api}\n")).expect("source.properties");
        std::fs::write(d.join("advancedFeatures.ini"), features).expect("features");
        s.env.create_avd(&NewAvd::for_image(name, &rel)).expect("create");
    }

    #[test]
    fn images_without_bootconfig_get_stocks_legacy_command_line() {
        let s = sdk("legacy");
        add_image(&s, "q", 29, "GLAsyncSwap = on\n");
        add_image(&s, "s", 33, "AndroidbootProps2 = on\n");

        // The 4.14 kernel would take a bootconfig block after the ramdisk as junk and panic.
        let q = plan(&s, "q");
        assert!(q.bootconfig.entries().is_empty(), "{:?}", q.bootconfig.entries());
        let words: Vec<&str> = q.cmdline.split(' ').collect();
        assert!(!words.contains(&"bootconfig"), "{}", q.cmdline);
        // As the stock emulator passes them to the same API 29 image.
        for w in [
            "android.qemud=1",
            "qemu.encrypt=1",
            "androidboot.hardware=ranchu",
            "androidboot.boot_devices=a003600.virtio_mmio",
            "qemu=1",
            "qemu.avd_name=q",
            "qemu.dalvik.vm.heapsize=576m",
            "qemu.opengles.version=196609",
            "qemu.vsync=60",
            "qemu.settings.system.screen_off_timeout=2147483647",
        ] {
            assert!(words.contains(&w), "{w} missing from {}", q.cmdline);
        }
        assert!(!q.cmdline.contains("androidboot.qemu"), "{}", q.cmdline);
        // logcat -Q's filter and pipe (#38), silent without -logcat.
        let cmdline_prop = |p: &BootPlan| p.boot_props.iter().find(|(k, _)| k == "qemu.cmdline").map(|(_, v)| v.clone());
        assert_eq!(cmdline_prop(&q).as_deref(), Some("androidboot.logcat=*:S androidboot.consolepipe=qemu_pipe,pipe:logcat"));
        let tags = Opts { logcat: Some("ActivityManager:I  *:S".into()), ..Opts::default() };
        let q_tags = BootPlan::new(&s.env.load("q").expect("load"), &tags, None);
        assert_eq!(
            cmdline_prop(&q_tags).as_deref(),
            Some("androidboot.logcat=ActivityManager:I,*:S androidboot.consolepipe=qemu_pipe,pipe:logcat")
        );

        let first: Vec<&str> = q.boot_props.iter().take(2).map(|(k, _)| k.as_str()).collect();
        assert_eq!(first, ["qemu.cmdline", "qemu.logcat"], "logcat -Q's property before the start that runs it");

        let new = plan(&s, "s");
        assert_eq!(cmdline_prop(&new), None, "bootconfig images have a logcat console");
        assert_eq!(new.bootconfig.get("androidboot.qemu.avd_name"), Some("s"));
        assert!(new.cmdline.split(' ').any(|w| w == "bootconfig") && !new.cmdline.contains("qemu.avd_name"), "{}", new.cmdline);
        let phone = plan(&s, "phone");
        assert_eq!(phone.bootconfig.get("androidboot.hardware"), Some("ranchu"), "no API level or flag: taken as new");
    }

    #[test]
    fn legacy_command_line_renames_graphics_and_drops_what_init_cannot_split() {
        let mut bc = BootConfig::new();
        bc.set("androidboot.hardware.gltransport", "virtio-gpu-asg")
            .set("androidboot.hardwareegl", "angle")
            .set("androidboot.qemu.adb.pubkey", "QAAAAB== user@host")
            .set("androidboot.vbmeta.size", "4352");
        for (k, v) in guest_graphics("angle", AngleImage { virtual_queue: true, guest_angle: true }) {
            bc.set(k, v);
        }
        let line = legacy_cmdline(&bc, true);
        // No ANGLE on these images: neither its driver nor what the UI draws with under it.
        assert_eq!(line, "android.qemud=1 qemu.encrypt=1 qemu.gles=1 qemu.gltransport=pipe androidboot.vbmeta.size=4352");
        assert!(!legacy_cmdline(&bc, false).contains("qemu.gles"));
    }

    #[test]
    fn guest_angle_draws_the_ui_with_vulkan_where_the_image_maps_it_as_stock_does() {
        let vvq = AngleImage { virtual_queue: true, guest_angle: false };
        let xr = AngleImage { virtual_queue: false, guest_angle: true };
        let get = |egl: &str, image: AngleImage, k: &str| guest_graphics(egl, image).into_iter().find(|(key, _)| *key == k).map(|(_, v)| v);
        // An API 36 image (VulkanVirtualQueue): stock's GuestAngle bootconfig on the same image.
        assert_eq!(get("angle", vvq, "androidboot.hardwareegl").as_deref(), Some("angle"));
        assert_eq!(get("angle", vvq, "androidboot.debug.hwui.renderer").as_deref(), Some("skiavk"));
        assert_eq!(get("angle", vvq, "androidboot.debug.renderengine.backend").as_deref(), Some("skiavkthreaded"));
        assert_eq!(get("angle", vvq, "androidboot.hardware.angle_feature_overrides_disabled").as_deref(), Some("exposeN*"));
        assert_eq!(get("angle", vvq, "androidboot.hardware.angle_feature_overrides_enabled").as_deref(), Some(""));
        assert!(virtual_queue("angle", vvq));
        // google-xr 34 (GuestAngle, no VulkanVirtualQueue): stock keeps HWUI on Skia GL and
        // leaves RenderEngine to the image.
        assert_eq!(get("angle", xr, "androidboot.debug.hwui.renderer").as_deref(), Some("skiagl"));
        assert_eq!(get("angle", xr, "androidboot.debug.renderengine.backend"), None);
        assert_eq!(get("angle", xr, "androidboot.hardware.angle_feature_overrides_disabled").as_deref(), Some("exposeN*"));
        assert!(!virtual_queue("angle", xr));
        // API 34 and 35 phones and Automotive advertise neither: the image's own choice.
        assert_eq!(guest_graphics("angle", AngleImage::default()), vec![("androidboot.hardwareegl", "angle".to_string())]);
        assert!(!virtual_queue("angle", AngleImage::default()));
        // The GLES encoder (EMU_GUEST_EGL=emulation) keeps the image's own choice and one
        // queue, as stock without GuestAngle does.
        for image in [vvq, xr, AngleImage::default()] {
            assert_eq!(guest_graphics("emulation", image), vec![("androidboot.hardwareegl", "emulation".to_string())]);
            assert!(!virtual_queue("emulation", image));
        }
    }

    #[test]
    fn host_angle_is_swiftshader_for_an_api_29_first_boot_and_a_renderer_without_gl() {
        assert_eq!(host_angle(Some(29), true, false), Some("swiftshader"));
        assert_eq!(host_angle(Some(29), false, false), None);
        for api in [Some(30), Some(33), Some(36), None] {
            assert_eq!(host_angle(api, true, false), None, "{api:?}");
            // Without GL nothing draws with the host's GLES, so its init cost picks the backend.
            assert_eq!(host_angle(api, true, true), Some("swiftshader"), "{api:?}");
            assert_eq!(host_angle(api, false, true), Some("swiftshader"), "{api:?}");
        }
    }

    #[test]
    fn the_host_gpu_driver_per_api_is_kosmickrisp_only_on_api_35_where_the_ui_is_skia_gl_on_angle() {
        use emu_gfxstream::Icd::{KosmicKrisp, MoltenVk};
        let s = sdk("gpu-icd");
        // The form-factor images with the API level a real one states (the selection reads it).
        for (dir, api) in [
            ("android-36/android-wear-signed", 36),
            ("android-34/android-tv", 34),
            ("android-34-ext9/android-automotive", 34),
            ("android-34/google-xr", 34),
        ] {
            let props = s.root.join("sdk/system-images").join(dir).join("arm64-v8a/source.properties");
            std::fs::write(props, format!("AndroidVersion.ApiLevel={api}\n")).expect("source.properties");
        }
        add_image(&s, "r", 30, "");
        add_image(&s, "t", 33, "AndroidbootProps2 = on\n");
        add_image(&s, "u", 34, "AndroidbootProps2 = on\n");
        add_image(&s, "v", 35, "AndroidbootProps2 = on\n");
        add_image(&s, "w", 36, "AndroidbootProps2 = on\nVulkanVirtualQueue = on\n");
        let icd = |name: &str, kosmickrisp: bool, asked: Option<&str>| {
            let avd = s.env.load(name).expect("load");
            host_gpu_icd(&avd, default_egl(avd.api_level()), kosmickrisp, asked)
        };
        // API 30 to 33: the GLES encoder. API 34 and 35: Skia GL on the guest's ANGLE, whose
        // pipelines MoltenVK can't compile (#412); KosmicKrisp draws API 35 but leaves API 34's
        // screen blank (#495), so API 34 stays on MoltenVK. API 36: Skia on Vulkan (skiavk).
        let want = [("r", 30, MoltenVk), ("t", 33, MoltenVk), ("u", 34, MoltenVk), ("v", 35, KosmicKrisp), ("w", 36, MoltenVk)];
        let wrong: Vec<String> = want
            .iter()
            .filter(|(name, _, want)| icd(name, true, None) != *want)
            .map(|(name, api, want)| format!("API {api}: {:?}, want {want:?}", icd(name, true, None)))
            .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
        // Automotive 34, google-xr 34 and Google TV 34 draw the same way as the API 34 phone, so
        // they stay on MoltenVK with it. An API 36 image that doesn't advertise
        // VulkanVirtualQueue (Wear OS 36) stays on MoltenVK, as every API 36 image did before #412.
        for name in ["car", "headset", "tv", "watch"] {
            assert_eq!(icd(name, true, None), MoltenVk, "{name}");
        }
        // ANDROID_EMU_VK_ICD=kosmickrisp still puts API 34 (and its form factors) on KosmicKrisp
        // where the renderer ships it, for investigating #495.
        for name in ["u", "car", "headset", "tv"] {
            assert_eq!(icd(name, true, Some("kosmickrisp")), KosmicKrisp, "{name}");
            assert_eq!(icd(name, false, Some("kosmickrisp")), MoltenVk, "{name}");
        }
        // A renderer without KosmicKrisp, or ANDROID_EMU_VK_ICD=moltenvk, keeps MoltenVK.
        for (name, ..) in want {
            assert_eq!(icd(name, false, None), MoltenVk, "{name}");
            assert_eq!(icd(name, true, Some("moltenvk")), MoltenVk, "{name}");
        }
        // The GLES encoder on an API 35 image (EMU_GUEST_EGL=emulation) has no guest ANGLE.
        let v = s.env.load("v").expect("load");
        assert_eq!(host_gpu_icd(&v, "emulation", true, None), MoltenVk);
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn kosmickrisp_falls_back_to_moltenvk_unless_its_probe_passes() {
        use emu_gfxstream::Icd::{KosmicKrisp, MoltenVk};

        use crate::vkprobe::Verdict;
        assert_eq!(checked_host_icd(KosmicKrisp, || Verdict::Pass), (KosmicKrisp, None));
        let (icd, notice) = checked_host_icd(KosmicKrisp, || Verdict::Fail);
        assert_eq!(icd, MoltenVk);
        assert!(notice.is_some_and(|n| n.contains("using MoltenVK")));
        // A hung or crashed probe: MoltenVK for this boot, and the notice says why.
        let (icd, notice) = checked_host_icd(KosmicKrisp, || Verdict::Unclear("timed out after 10.0 s; killed pid 7".into()));
        assert_eq!(icd, MoltenVk);
        assert!(notice.is_some_and(|n| n.contains("timed out") && n.contains("using MoltenVK for this boot")));
        // MoltenVK needs no KosmicKrisp probe.
        assert_eq!(checked_host_icd(MoltenVk, || panic!("probed KosmicKrisp for a MoltenVK boot")), (MoltenVk, None));
    }

    #[test]
    fn images_below_api_34_get_the_gles_encoder() {
        // API 31–33's ANGLE gives SurfaceFlinger no EGL config on the renderer's Vulkan.
        for api in [31, 32, 33] {
            assert_eq!(default_egl(Some(api)), "emulation", "API {api}");
        }
        for api in [Some(34), Some(36), None] {
            assert_eq!(default_egl(api), "angle", "{api:?}");
        }
    }

    #[test]
    fn a_plan_with_a_renderer_tells_the_guest_its_graphics() {
        let s = sdk("graphics");
        add_image(&s, "v", 35, "AndroidbootProps2 = on\n");
        let with =
            |name: &str| BootPlan::new(&s.env.load(name).expect("load"), &Opts::default(), Some(PathBuf::from("/nonexistent/lib64")));
        let (hwui, renderengine) = ("androidboot.debug.hwui.renderer", "androidboot.debug.renderengine.backend");
        let phone = with("phone");
        assert_eq!(phone.bootconfig.get("androidboot.hardware.gltransport"), Some("virtio-gpu-asg"));
        assert!(phone.bootconfig.get("androidboot.hardwareegl").is_some());
        if guest_egl(&s.env.load("phone").expect("load")) == "angle" {
            // API 36 (VulkanVirtualQueue): Skia on Vulkan.
            assert_eq!(phone.bootconfig.get(hwui), Some("skiavk"));
            assert_eq!(phone.bootconfig.get(renderengine), Some("skiavkthreaded"));
            assert_eq!(phone.bootconfig.get("androidboot.hardware.angle_feature_overrides_disabled"), Some("exposeN*"));
            // google-xr 34 (GuestAngle): Skia GL, as stock gives it.
            let headset = with("headset");
            assert_eq!(headset.bootconfig.get(hwui), Some("skiagl"));
            assert_eq!(headset.bootconfig.get(renderengine), None);
        }
        // API 34 and 35 images and Automotive advertise neither: none of the keys.
        for name in ["v", "tv", "car"] {
            let p = with(name);
            for k in ANGLE_UI_KEYS {
                assert_eq!(p.bootconfig.get(k), None, "{name}: {k}");
            }
        }
        let without = plan(&s, "phone");
        assert_eq!(without.bootconfig.get(hwui), None, "no renderer: the image's default");
    }

    #[test]
    fn images_below_api_29_are_refused_with_the_reason() {
        let s = sdk("old");
        add_image(&s, "p", 28, "");
        add_image(&s, "q", 29, "");
        add_image(&s, "r", 30, "");
        add_image(&s, "s", 31, "AndroidbootProps2 = on\n");
        add_image(&s, "u", 34, "AndroidbootProps2 = on\n");
        let why = |n: &str| unsupported(&s.env.load(n).expect("load"));
        let w = why("p").unwrap_or_default();
        assert!(w.starts_with("p: API 28 images") && w.contains("API 29 and newer"), "{w}");
        // API 29–32 draw through the opengles pipe (#38, #64); API 33 through translated ASG (#39).
        assert_eq!((why("q"), why("r"), why("s"), why("u")), (None, None, None, None));
        assert_eq!(why("phone"), None, "no API level: not refused");
    }

    #[test]
    fn pipe_images_get_no_virtio_gpu() {
        let s = sdk("pipe-gles");
        add_image(&s, "q", 29, "");
        add_image(&s, "s", 31, "AndroidbootProps2 = on\n");
        add_image(&s, "s2", 32, "AndroidbootProps2 = on\n");
        add_image(&s, "t", 33, "AndroidbootProps2 = on\n");
        let gpu = Some(PathBuf::from("/renderer/lib64"));
        let with = |n: &str| BootPlan::new(&s.env.load(n).expect("load"), &Opts::default(), gpu.clone());
        let transport = |p: &BootPlan| p.bootconfig.get("androidboot.hardware.gltransport").map(str::to_string);
        let q = with("q");
        assert!(q.pipe_gles);
        assert!(q.devices().iter().any(|d| d.starts_with("goldfish pipe (opengles")), "{:?}", q.devices());
        assert!(!q.devices().iter().any(|d| d.contains("virtio-gpu")), "{:?}", q.devices());
        // API 31 and 32 (#64): their media.swcodec may not open the GPU render node, so the
        // GLES encoder in it (the gralloc mapper's CPU reads) goes over the pipe, as stock's.
        for n in ["s", "s2"] {
            let p = with(n);
            assert!(p.pipe_gles, "{n}");
            assert!(!p.devices().iter().any(|d| d.contains("virtio-gpu")), "{n}: {:?}", p.devices());
            assert_eq!(transport(&p).as_deref(), Some("pipe"), "{n}");
            assert_eq!(p.bootconfig.get("androidboot.qemu.gltransport.name"), Some("pipe"), "{n}");
        }
        // API 33 keeps virtio-gpu: its policy lets media.swcodec open the render node.
        let t = with("t");
        assert!(!t.pipe_gles);
        assert!(t.devices().iter().any(|d| d.starts_with("virtio-gpu")), "{:?}", t.devices());
        assert_eq!(transport(&t).as_deref(), Some("virtio-gpu-asg"));
    }

    #[test]
    fn a_logcat_filter_too_long_for_the_property_shows_every_tag() {
        let fits = logcat_pipe_cmdline("ActivityManager:I *:S");
        assert!(fits.len() <= PROP_VALUE_MAX && fits.contains("ActivityManager:I,*:S"), "{fits}");
        // 65 bytes around the filter: 26 fit, 27 don't.
        let at_cap = "A".repeat(24) + ":I";
        assert_eq!(logcat_pipe_cmdline(&at_cap).len(), PROP_VALUE_MAX);
        let long = logcat_pipe_cmdline(&("A".repeat(25) + ":I"));
        assert_eq!(long, "androidboot.logcat=*:V androidboot.consolepipe=qemu_pipe,pipe:logcat");
    }

    fn plan(s: &Sdk, name: &str) -> BootPlan {
        BootPlan::new(&s.env.load(name).expect("load"), &Opts::default(), None)
    }

    #[test]
    fn logcat_files_rotate_at_their_cap() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("emu-logcat-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("logcat.txt");
        std::fs::write(&path, "old run\n").expect("old");
        let mut f = RotatingFile::create(&path, 10).expect("create");
        assert_eq!(std::fs::read(&path).expect("read"), b"", "a new process starts the file again");
        f.write_all(b"aaaa\n").and_then(|()| f.write_all(b"bbbb\n")).expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"aaaa\nbbbb\n");
        f.write_all(b"cccc\n").expect("write");
        assert_eq!(std::fs::read(dir.join("logcat.txt.1")).expect("rotated"), b"aaaa\nbbbb\n");
        assert_eq!(std::fs::read(&path).expect("read"), b"cccc\n");
        // A chunk bigger than the cap still goes in whole, into a fresh file.
        f.write_all(&[b'x'; 25]).expect("write");
        assert_eq!(std::fs::read(dir.join("logcat.txt.1")).expect("rotated"), b"cccc\n");
        assert_eq!(std::fs::read(&path).expect("read").len(), 25);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A guest reset builds a new machine config; what the boot before it logged (a panic's
    /// text) stays in console.log and the logcat file.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn a_reset_keeps_the_logs_of_the_boot_before() {
        let s = sdk("reset-logs");
        let avd = s.env.load("phone").expect("load");
        let logcat = s.root.join("logcat.txt");
        // A port outside 5554..5682, so no real emulator or farm is affected by its claim.
        let opts = Opts { port: Some(6990), gpu: Some("off".into()), logcat_output: Some(logcat.display().to_string()), ..Opts::default() };
        let inst = Instance::prepare(&avd, &opts).expect("prepare");
        let launched = std::time::Instant::now();
        let mut first = inst.machine_config(&opts, launched).expect("config");
        first.console.write_all(b"Kernel panic - not syncing: sysrq triggered crash\n").expect("console");
        first.logcat.as_ref().expect("logcat").send(b"F/libc: Fatal signal 6\n");
        drop(first);
        let mut second = inst.machine_config(&opts, launched).expect("config after reset");
        second.console.write_all(b"Booting Linux\n").expect("console");
        second.logcat.as_ref().expect("logcat").send(b"I/init: second boot\n");
        let console = std::fs::read_to_string(emu_avd::state_dir(&avd.dir).join("console.log")).expect("console.log");
        assert_eq!(console, "Kernel panic - not syncing: sysrq triggered crash\nBooting Linux\n");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&logcat).unwrap_or_default() != "F/libc: Fatal signal 6\nI/init: second boot\n" {
            assert!(std::time::Instant::now() < deadline, "{:?}", std::fs::read_to_string(&logcat));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop((second, inst));
        if let Some(locks) = emu_avd::port_lock_dir() {
            let _ = std::fs::remove_file(locks.join("6990.lock"));
        }
    }

    #[test]
    fn parallel_launches_claim_distinct_ports() {
        let dir = std::env::temp_dir().join(format!("emu-ports-{}", std::process::id()));
        // Sixteen launches at once, every pair probing free: each gets its own pair.
        let picked: Vec<(u16, std::fs::File)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16).map(|_| s.spawn(|| pick_port(&dir, 5600..=5682, |_| true).expect("a port"))).collect();
            handles.into_iter().map(|h| h.join().expect("thread")).collect()
        });
        let mut ports: Vec<u16> = picked.iter().map(|(p, _)| *p).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 16, "{ports:?}");
        assert!(ports.iter().all(|p| p % 2 == 0 && (5600..=5630).contains(p)), "{ports:?}");
        // A held claim is skipped and an explicit claim of it refused; a pair in use (a stock
        // emulator's) is skipped too. Dropping a claim frees its port.
        assert!(claim_port(&dir, 5600).is_none());
        assert_eq!(pick_port(&dir, 5600..=5682, |p| p != 5633).map(|(p, _)| p), Some(5634));
        drop(picked);
        assert_eq!(pick_port(&dir, 5600..=5682, |_| true).map(|(p, _)| p), Some(5600));
        assert!(pick_port(&dir, 5600..=5602, |_| false).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_form_factor_gets_its_devices() {
        let s = sdk("devices");
        let shape = |p: &BootPlan| (p.touch_screen, p.touchpad, p.dual_mode_mouse, p.xr, p.car, p.sound);
        assert_eq!(shape(&plan(&s, "phone")), (true, None, false, false, false, true), "the image enables VirtioSndCard");
        assert_eq!(shape(&plan(&s, "watch")), (true, None, false, false, false, true));
        assert_eq!(shape(&plan(&s, "tv")), (false, None, false, false, false, true), "no touch screen");
        assert_eq!(shape(&plan(&s, "car")), (true, None, false, false, true, true), "the VHAL and a sound card");
        assert_eq!(shape(&plan(&s, "headset")), (true, None, true, true, false, true));
        assert_eq!(shape(&plan(&s, "glasses")), (true, Some((1542, 297)), false, true, false, true), "a touchpad, no mouse");

        let tv = plan(&s, "tv").devices().join(", ");
        assert!(tv.contains("virtio-input: keyboard (qwerty2), rotary") && !tv.contains("multi-touch"), "{tv}");
        let glasses = plan(&s, "glasses").devices().join(", ");
        assert!(glasses.contains("virtio-input: touchpad") && !glasses.contains("dual-mode mouse"), "{glasses}");
        assert!(plan(&s, "car").devices().contains(&"qemud:car (vehicle HAL)"));
        assert!(plan(&s, "headset").devices().contains(&"qemud:xr_service"));
    }

    #[test]
    fn a_pixel_fold_gets_its_images_device_states_and_skin_overlays() {
        let s = sdk("fold");
        let rel = "system-images/android-36/google_apis/arm64-v8a";
        let image = s.root.join("sdk").join(rel);
        std::fs::write(image.join("advancedFeatures.ini"), "VirtioSndCard = on\nDeviceStateOnBoot = on\nDeviceSkinOverlay = on\n")
            .expect("features");
        let states = image.join("data/misc/pixel_fold/devicestate");
        std::fs::create_dir_all(&states).expect("mkdir");
        std::fs::write(states.join("device_state_configuration.xml"), "<device-state-config>\n  <device-state/>\n</device-state-config>\n")
            .expect("xml");
        let fold = sim_core::form_factor::Kind::new(sim_core::platform::Platform::Android, sim_core::form_factor::FormFactor::Foldable)
            .expect("fold")
            .profile();
        s.env.create_avd(&NewAvd::for_profile("pf", rel, &fold)).expect("create");
        let avd = s.env.load("pf").expect("load");
        let p = plan(&s, "pf");
        assert_eq!(p.bootconfig.get("androidboot.qemu.device_state"), Some("<device-state-config><device-state/></device-state-config>"));
        assert_eq!(
            crate::fold::skin_overlays(&avd),
            [
                "cmd overlay enable com.android.internal.emulation.pixel_fold",
                "cmd overlay enable com.android.systemui.emulation.pixel_fold"
            ]
        );
        // A phone on the same image gets neither.
        assert_eq!(plan(&s, "phone").bootconfig.get("androidboot.qemu.device_state"), None);
        assert!(crate::fold::skin_overlays(&s.env.load("phone").expect("load")).is_empty());
        // Nor does a foldable whose image doesn't ship its device.
        s.env
            .create_avd(&NewAvd::for_profile("duo", rel, &fold.clone().with_screen(2784, 1800, sim_core::form_factor::Density::Dpi(401))))
            .expect("duo");
        assert_eq!(plan(&s, "duo").bootconfig.get("androidboot.qemu.device_state"), None);
    }

    #[test]
    fn plans_tell_the_guest_its_screen_and_shape() {
        let s = sdk("boot");
        let watch = plan(&s, "watch");
        assert_eq!((watch.screen, watch.ram_bytes), ((454, 454, 320), 1 << 30), "512 MB, booted with stock's padding");
        let bc = |p: &BootPlan, k: &str| p.bootconfig.get(k).map(str::to_owned);
        assert_eq!(bc(&watch, "androidboot.emulator.circular").as_deref(), Some("1"));
        assert_eq!(bc(&watch, "androidboot.qemu.skin").as_deref(), Some("454x454"));
        assert_eq!(bc(&watch, "androidboot.qemu.avd_name").as_deref(), Some("watch"));
        assert_eq!(bc(&watch, "androidboot.vbmeta.size").as_deref(), Some("6592"));
        assert_eq!(bc(&watch, "androidboot.hardware.gltransport"), None, "no renderer, no graphics transport");
        assert!(watch.boot_props.contains(&("qemu.hw.mainkeys".into(), "1".into())));
        assert_eq!(bc(&plan(&s, "phone"), "androidboot.emulator.circular"), None);

        let with_gpu = BootPlan::new(&s.env.load("phone").expect("load"), &Opts::default(), Some(PathBuf::from("/r/lib64")));
        assert_eq!(bc(&with_gpu, "androidboot.hardware.gltransport").as_deref(), Some("virtio-gpu-asg"));
        assert!(with_gpu.devices().iter().any(|d| d.starts_with("virtio-gpu")));

        let glasses = plan(&s, "glasses");
        assert!(
            glasses.cmdline.starts_with(BASE_CMDLINE) && glasses.cmdline.ends_with(" 8250.nr_uarts=1"),
            "the image's cmdline is appended"
        );
        assert_eq!((glasses.screen, glasses.ram_bytes), ((450, 450, 160), 2 << 30));
        assert_eq!(plan(&s, "car").ram_bytes, 3686 << 20);
        assert_eq!(plan(&s, "car").kind, "Android Automotive · Auto");
    }

    /// Only phones, tablets and foldables follow their guest's rotation: an XR headset, display
    /// glasses, a TV, a car and a watch log nothing for it and get no watch on their log.
    #[test]
    fn only_displays_that_turn_follow_the_guests_rotation() {
        let s = sdk("turns");
        for name in ["watch", "tv", "car", "headset", "glasses"] {
            let avd = s.env.load(name).expect("load");
            assert!(!follows_rotation(&avd), "{name}");
            assert_eq!(BootPlan::new(&avd, &Opts::default(), None).bootconfig.get("androidboot.logcat"), Some("*:S"), "{name}");
            let mine = Opts { logcat: Some("MyApp:D *:S".into()), ..Opts::default() };
            assert_eq!(BootPlan::new(&avd, &mine, None).bootconfig.get("androidboot.logcat"), Some("MyApp:D *:S"), "{name}");
        }
        let phone = s.env.load("phone").expect("load");
        assert!(follows_rotation(&phone));
        let mine = Opts { logcat: Some("MyApp:D *:S".into()), ..Opts::default() };
        assert_eq!(
            BootPlan::new(&phone, &mine, None).bootconfig.get("androidboot.logcat"),
            Some("MyApp:D WindowManager:I ActivityTaskManager:I *:S")
        );
        // Without a display to turn or output to write, there's no stream to keep.
        assert!(logcat_sink(&Opts::default(), None).expect("sink").is_none());
    }

    /// API 29 and 30 take their logcat filter through `qemu.cmdline`, at most `PROP_VALUE_MAX`
    /// bytes (a longer value isn't set, and logcat -Q logs nothing): emu adds nothing to it there
    /// and doesn't follow their rotation, so the property is main's, the default and a typical
    /// `-logcat` fit, and `rotation` says it doesn't know. API 31 and later read it from the log.
    #[test]
    fn images_whose_log_goes_through_qemu_cmdline_keep_the_users_filter() {
        let s = sdk("cmdline-rotation");
        add_image(&s, "q", 29, "");
        add_image(&s, "r", 30, "");
        add_image(&s, "s", 31, "AndroidbootProps2 = on\n");
        let prop = |name: &str, opts: &Opts| {
            let p = BootPlan::new(&s.env.load(name).expect("load"), opts, None);
            p.boot_props.iter().find(|(k, _)| k == "qemu.cmdline").map(|(_, v)| v.clone())
        };
        let typical = Opts { logcat: Some("ActivityManager:I *:S".into()), ..Opts::default() };
        let to_file = Opts { logcat_output: Some("/tmp/l".into()), ..Opts::default() };
        for name in ["q", "r"] {
            let avd = s.env.load(name).expect("load");
            assert!(sim_core::rotation::turns(avd.profile().kind.form_factor), "{name}: a phone");
            assert!(!follows_rotation(&avd) && rotation_unknown(&avd), "{name}");
            for (opts, filter) in [(&Opts::default(), "*:S"), (&typical, "ActivityManager:I,*:S"), (&to_file, "*:V")] {
                let line = prop(name, opts).expect("qemu.cmdline");
                assert!(line.len() <= PROP_VALUE_MAX, "{name}: {} bytes: {line}", line.len());
                assert_eq!(line, format!("androidboot.logcat={filter} androidboot.consolepipe=qemu_pipe,pipe:logcat"), "{name}");
            }
        }
        let s31 = s.env.load("s").expect("load");
        assert!(follows_rotation(&s31) && !rotation_unknown(&s31));
        assert_eq!(prop("s", &Opts::default()), None);
        assert_eq!(
            BootPlan::new(&s31, &typical, None).bootconfig.get("androidboot.logcat"),
            Some("ActivityManager:I WindowManager:I ActivityTaskManager:I *:S")
        );
        // A display that never turns is upright, not unknown.
        assert!(!rotation_unknown(&s.env.load("tv").expect("load")));
    }

    /// The rotation watch reads the guest's log as it's sent, not behind the spool's queue (whose
    /// writer thread runs at utility priority and drops chunks when it falls behind): the turn
    /// has happened by the time `send` returns.
    #[test]
    fn the_rotation_watch_reads_each_line_as_it_is_sent() {
        use sim_core::rotation::Rotation;
        let display = std::sync::Arc::new(emu_vmm::display::FrameStore::default());
        let spool = logcat_sink(&Opts::default(), Some(display.clone())).expect("sink").expect("a stream to watch");
        for (line, want) in [
            ("11-26 09:58:57.210   642   680 I WindowManager: Display id=0 rotation changed to 1 from 0\n", Rotation::Deg270),
            ("11-26 09:58:58.210   642   680 I WindowManager: Display id=0 rotation changed to 0 from 1\n", Rotation::Deg0),
            ("11-26 09:58:59.210   642   680 I WindowManager: Display id=0 rotation changed to 3 from 0\n", Rotation::Deg90),
        ] {
            spool.send(line.as_bytes());
            assert_eq!(display.rotation(), want, "{line}");
        }
        assert_eq!(spool.dropped(), 0);
    }

    /// The guest's log turns the display as each rotation line arrives (split across writes or
    /// not), and the user's output keeps only what their filter shows.
    #[test]
    fn the_guests_log_turns_the_display_and_the_user_sees_their_own_lines() {
        use sim_core::rotation::Rotation;
        use std::io::Write as _;
        #[derive(Clone, Default)]
        struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("out").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let display = std::sync::Arc::new(emu_vmm::display::FrameStore::default());
        let out = Shared::default();
        let user = sim_core::logcat::Filter::parse("MyApp:D *:S");
        let mut watch = rotation_watch(display.clone());
        let mut shown = UserLines { out: Box::new(out.clone()), filter: user, lines: Lines::default() };
        let mut send = |b: &[u8]| {
            watch(b);
            shown.write_all(b).expect("write");
        };
        let turn = "11-26 09:58:57.210   642   680 I WindowManager: Display id=0 rotation changed to 3 from 0, lastOrientation=-1\n";
        let (a, b) = turn.split_at(60);
        send(a.as_bytes());
        assert_eq!(display.rotation(), Rotation::Deg0, "half a line");
        send(format!("{b}11-26 09:58:57.300  4242  4242 D MyApp: hello\n11-26 09:58:57.301  4242  4242 V MyApp: quiet\n").as_bytes());
        assert_eq!(display.rotation(), Rotation::Deg90);
        assert_eq!(String::from_utf8_lossy(&out.0.lock().expect("out")), "11-26 09:58:57.300  4242  4242 D MyApp: hello\n");
        // API 34's line, back upright.
        send(
            b"11-26 10:07:09.487   540  1888 I ActivityTaskManager: Config changes=20000580 {winConfig={ mDisplayRotation=ROTATION_0 }}\n",
        );
        assert_eq!(display.rotation(), Rotation::Deg0);
        // An app that logs the same words under its own tag turns nothing.
        send(b"11-26 10:07:10.000  4242  4242 I MyApp: Display id=0 rotation changed to 1 from 0\n");
        assert_eq!(display.rotation(), Rotation::Deg0);
    }

    #[test]
    fn plans_follow_the_command_line_and_find_missing_files() {
        let s = sdk("opts");
        let avd = s.env.load("phone").expect("load");
        let opts = Opts { cores: Some(2), memory_mb: Some(1536), read_only: true, no_boot_anim: true, ..Opts::default() };
        let p = BootPlan::new(&avd, &opts, None);
        assert_eq!((p.cpus, p.ram_bytes), (2, 1536 << 20));
        assert!(p.disks.iter().all(|d| d.mode == DiskMode::Ephemeral), "-read-only keeps every write in memory");
        assert_eq!(p.bootconfig.get("androidboot.debug.sf.nobootanimation"), Some("1"));
        assert!(p.missing().is_empty(), "the AVD's own disks are made at boot");

        // The console is quiet unless -show-kernel; logcat's stream is silent unless asked for,
        // but for the lines that say the display rotated.
        let p = BootPlan::new(&avd, &Opts::default(), None);
        assert!(p.cmdline.contains(" loglevel=3") && p.cmdline.contains("console=ttyAMA0"), "{}", p.cmdline);
        assert_eq!(p.bootconfig.get("androidboot.logcat"), Some("WindowManager:I ActivityTaskManager:I *:S"));
        let shown = BootPlan::new(&avd, &Opts { show_kernel: true, logcat: Some("*:I".into()), ..Opts::default() }, None);
        assert!(!shown.cmdline.contains("loglevel="), "{}", shown.cmdline);
        assert_eq!(shown.bootconfig.get("androidboot.logcat"), Some("*:I"));
        let to_file = BootPlan::new(&avd, &Opts { logcat_output: Some("/tmp/l".into()), ..Opts::default() }, None);
        assert_eq!(to_file.bootconfig.get("androidboot.logcat"), Some("*:V"));

        let userdata = p.disks.iter().find(|d| d.serial == "userdata").expect("userdata");
        assert_eq!((userdata.slot, userdata.mode), (slots::USERDATA, DiskMode::Persistent));
        std::fs::remove_file(avd.vendor_image()).expect("rm");
        assert_eq!(p.missing(), [avd.vendor_image().as_path()]);

        let text = p.to_string();
        for line in [
            "avd       phone (Android · Phone)",
            "machine   4 vCPUs, 2048 MB RAM, 720x1280 @ 320 dpi",
            "  userdata  slot 29 ",
            "bootconfig\n  androidboot.hardware=ranchu\n",
        ] {
            assert!(text.contains(line), "{line:?} missing from\n{text}");
        }
    }

    /// x86_64 images get stock's x86 board (software mode, #55): the console on the first 8250
    /// UART and the disks on PCI, where the image's fstab looks for them.
    #[test]
    fn an_x86_64_image_gets_stocks_x86_board() {
        let s = sdk("x86");
        let boot_devices =
            |p: &BootPlan| p.bootconfig.entries().iter().find(|(k, _)| k == "androidboot.boot_devices").map(|(_, v)| v.clone());
        let x86 = BootPlan::new(&s.env.load("x86").expect("load"), &Opts::default(), Some(PathBuf::from("/r/lib64")));
        let arm = BootPlan::new(&s.env.load("phone").expect("load"), &Opts::default(), None);
        assert!(x86.cmdline.starts_with("8250.nr_uarts=1 no_timer_check console=ttyS0,38400 "), "{}", x86.cmdline);
        assert!(!x86.cmdline.contains("ttyAMA0"), "{}", x86.cmdline);
        // KFENCE's sampling flips a static key twice a second, patching kernel text that TCG's
        // other vCPUs may be running.
        assert!(x86.cmdline.contains(" kfence.sample_interval=0"), "{}", x86.cmdline);
        assert!(!arm.cmdline.contains("kfence"), "only software mode needs it: {}", arm.cmdline);
        assert_eq!(boot_devices(&x86).as_deref(), Some(emu_vmm::layout::x86::BOOT_DEVICES));
        assert!(x86.devices().iter().any(|d| d.contains("QEMU")), "{:?}", x86.devices());
        assert!(x86.to_string().contains("userdata  pci 00:05.0 "), "{x86}");
        assert_eq!(boot_devices(&arm).as_deref(), Some("a003600.virtio_mmio"));
        // The display's refresh rate is the AVD's `hw.lcd.vsync`, as stock reads it.
        let vsync = |p: &BootPlan| p.bootconfig.entries().iter().find(|(k, _)| k == "androidboot.qemu.vsync").map(|(_, v)| v.clone());
        assert_eq!(vsync(&x86).as_deref(), Some("60"));
        let config = s.env.load("x86").expect("load").dir.join("config.ini");
        let mut text = std::fs::read_to_string(&config).expect("config");
        text.push_str("hw.lcd.vsync=30\n");
        std::fs::write(&config, text).expect("config");
        let slow = BootPlan::new(&s.env.load("x86").expect("load"), &Opts::default(), None);
        assert_eq!(vsync(&slow).as_deref(), Some("30"));
        assert!(arm.cmdline.contains("console=ttyAMA0"));
        let _ = std::fs::remove_dir_all(&s.root);
    }
}
