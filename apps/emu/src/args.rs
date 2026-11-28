//! The SDK emulator's command line: single-dash long flags, some taking a value.
use anyhow::{Result, bail};
use emu_vmm::pipe::camera::{Facing, SourceSpec};

pub const USAGE: &str = "\
usage: emu -avd NAME [options]
       emu @NAME [options]
       emu -avd NAME -dry-run   (print the boot plan without booting; any host)
       emu -list-avds | -version | -accel-check
       emu -create-avd NAME -sysdir system-images/android-36/google_apis/arm64-v8a [-lcd 720x1280@320] [-cores N] [-memory MB] [-profile software-lite]
                  (-create-avd boots the first AVD of an image once and keeps its settled user data to
                   start the image's later AVDs from; -no-golden skips that for a pristine first boot)
       emu -clone-avd NAME -from GOLDEN   (copy-on-write clone of a stopped AVD's settled simee disks)
       emu -delete-avd NAME               (refused while any emulator, simee or stock, runs it)
       emu -provision API [-tag google_apis] [-abi arm64-v8a] [-no-renderer] [-dry-run] [-create-avd NAME [-lcd WxH@DPI] [-cores N] [-memory MB] [-profile software-lite]]
       emu -provision-renderer [-dry-run]
       emu -list-images [-tag TAG] [-abi arm64-v8a]

provisioning (no Java or sdkmanager; SDK at $ANDROID_HOME, default ~/Library/Android/sdk):
  -provision API    install platform-tools, system-images;android-API;TAG;ABI (API: 36, 36.1, ...)
                    and the gfxstream renderer (unless the SDK already has one)
  -provision-renderer
                    install or update just the renderer: lib64 of the emulator package, into
                    $ANDROID_HOME/simee/renderer/<revision>; both then initialize the renderer
                    once, so its Metal shaders are cached before the first boot
  -no-renderer      with -provision: skip the renderer (emu then boots without graphics)
  -profile NAME     with -create-avd: software-lite makes a phone for software mode (x86_64 Linux
                    without KVM): 540x960 at 240 dpi and a 30 Hz display (hw.lcd.vsync=30), for a
                    quieter host (docs/android.md, Software mode); an explicit -lcd wins
  -no-golden        with -create-avd: don't boot the image once to keep a settled golden user data
                    (the new AVD first-boots fresh, as stock's does)
  -tag TAG          default, google_apis (default), google_apis_playstore, android-wear, ...
  -abi ABI          default arm64-v8a
  -dry-run          resolve and print URL, size and sha1 without downloading; with -avd, print
                    the boot plan (cmdline, bootconfig, boot props, disks, devices) instead of booting
  -list-images      system images available for this host (all tags unless -tag)

options:
  -cores N          vCPUs (default: hw.cpu.ncore)
  -memory MB        guest RAM (default: hw.ramSize)
  -port N           console port; adb uses N+1
  -read-only        keep every disk write in memory (several instances per AVD)
  -wipe-data        reset user data before booting
  -show-kernel      print the guest console, every kernel and init message (without it the
                    console, <avd>/simee/console.log, gets only critical ones and panics, like
                    stock; EMU_CONSOLE=full logs everything there without printing it)
  -logcat TAGS      stream the guest's logcat (filter TAGS, e.g. '*:I') to stdout
  -logcat-output F  stream it to file F instead (all of it unless -logcat; F rolls over to F.1
                    at 4 MiB)
  -verbose          diagnostics, including vCPU exit counts at shutdown
  -no-boot-anim     skip the boot animation
  -accel MODE       software mode (x86_64 Linux): auto (default: KVM when /dev/kvm is usable,
                    else QEMU's TCG), on (KVM or fail), off (TCG); -no-accel is -accel off
  -disk-cache MODE  unsafe (default: guest flushes stay in the host page cache) or fsync
  -gpu MODE         rendering mode, default auto (host GPU through MoltenVK):
                      auto, host     host GPU through MoltenVK, software if that fails
                      lavapipe, swangle, angle_indirect
                                     software Vulkan (lavapipe): no GPU, CI, headless
                      swiftshader, swiftshader_indirect
                                     software Vulkan (SwiftShader)
                      software, guest
                                     lavapipe, with stock's notice: SDK images have no
                                     in-guest renderer, so guest rendering isn't possible
                      off, none      no renderer, no graphics
                      metal          host, and posted frames also go to simee as IOSurfaces
                                     copied on the GPU (experimental, macOS; issue #356)
                    An AVD's hw.gpu.mode (guest, software, lavapipe, ...) applies when -gpu
                    isn't given; the flag wins.
  -no-audio         the sound card stays silent and no host audio device is opened (-audio none)
  -allow-host-audio the guest records from the host's default microphone (otherwise silence)
  -netspeed S       initial network speed: gsm, hscsd, gprs, umts, edge, hsdpa, lte, evdo, 5g,
                    full (default), <kbps> or <up>:<down>; `network status` reports it, traffic
                    isn't shaped yet
  -netdelay D       initial network latency: gsm, gprs, edge, umts, ..., none (default), <ms> or
                    <min>:<max>; reported the same way
  -camera-back S    the back camera's source: emulated (the test card), webcam0 (the host's first
                    camera; webcam1, ...), none, or a simee source id (pattern:testcard, qr:TEXT,
                    text:TEXT, image:PATH, solid:COLOUR, webcam:NAME); default hw.camera.back
  -camera-front S   the same for the front camera (default hw.camera.front)
Other SDK emulator flags are accepted.
";

/// Whether this build boots in software mode, the only one `-accel` chooses for: on a Mac the
/// hypervisor is the only accelerator, so there `-accel` is listed as not implemented.
const SOFTWARE_MODE: bool = cfg!(all(target_os = "linux", target_arch = "x86_64"));

/// Flags that take a value and that we don't act on (yet).
const IGNORED_WITH_VALUE: &[&str] = &[
    "ports",
    "grpc",
    "packet-streamer-endpoint",
    "feature",
    "virtualscene-poster",
    "skin",
    "skindir",
    "dns-server",
    "http-proxy",
    "timezone",
    "shell-serial",
    "prop",
    "datadir",
    "sdcard",
    "partition-size",
    "snapshot",
    "id",
    "selinux",
    "screen",
    "grpc-use-token",
    "grpc-use-jwt",
    "metrics-collection",
    "metrics-to-file",
    "idle-grpc-timeout",
    "radio",
    "encryption-key",
    "cache",
    "cache-size",
    "kernel",
    "ramdisk",
    "system",
    "vendor",
    "data",
    "initdata",
];

#[derive(Debug, Default)]
pub struct Opts {
    pub avd: Option<String>,
    pub list_avds: bool,
    pub version: bool,
    pub help: bool,
    pub accel_check: bool,
    /// `-accel on|off|auto`, `-no-accel`: None is auto. Software mode only (Linux): on a Mac the
    /// hypervisor is the only accelerator.
    pub accel: Option<bool>,
    pub cores: Option<u32>,
    pub memory_mb: Option<u64>,
    pub port: Option<u16>,
    pub read_only: bool,
    pub wipe_data: bool,
    pub show_kernel: bool,
    /// `-logcat TAGS`: the guest's logcat filter, streamed to stdout.
    pub logcat: Option<String>,
    /// `-logcat-output FILE`: the stream goes to FILE.
    pub logcat_output: Option<String>,
    pub verbose: bool,
    pub no_boot_anim: bool,
    pub no_window: bool,
    /// `-no-audio` (or `-audio none`): the sound card neither plays nor records on the host.
    pub no_audio: bool,
    /// `-allow-host-audio`: the sound card records from the host's microphone.
    pub allow_host_audio: bool,
    /// `-gpu MODE`: `off`/`none` disables host rendering; anything else uses gfxstream.
    pub gpu: Option<String>,
    /// `-gpu metal`: the host renderer (`gpu` is `host`), with the Metal present path on.
    pub metal_present: bool,
    /// `-netspeed`, `-netdelay` (`-netfast`: both unlimited, the default).
    pub network: sim_core::network::Conditions,
    /// `-disk-cache unsafe|fsync`: whether guest disk flushes fsync on the host.
    pub disk_fsync: bool,
    pub create_avd: Option<String>,
    /// `-profile NAME` (one of `emu_avd::PROFILES`): what `-create-avd` makes.
    pub profile: Option<String>,
    /// `-clone-avd NAME -from GOLDEN`.
    pub clone_avd: Option<String>,
    pub from: Option<String>,
    pub delete_avd: Option<String>,
    pub sysdir: Option<String>,
    /// WxH@DPI
    pub lcd: Option<(u32, u32, u32)>,
    /// `-provision API`: install platform-tools, a system image and the renderer.
    pub provision: Option<String>,
    /// `-provision-renderer`: install or update only the gfxstream renderer.
    pub provision_renderer: bool,
    /// `-no-renderer`: `-provision` without the renderer.
    pub no_renderer: bool,
    /// `-no-golden`: `-create-avd` neither builds nor clones the image's golden user data.
    pub no_golden: bool,
    pub list_images: bool,
    pub tag: Option<String>,
    pub abi: Option<String>,
    pub dry_run: bool,
    /// `-camera-back`, `-camera-front`: the stock camera value or a simee source id, checked.
    pub camera_back: Option<String>,
    pub camera_front: Option<String>,
    /// Flags accepted for compatibility but not implemented.
    pub ignored: Vec<String>,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if let Some(name) = arg.strip_prefix('@') {
            o.avd = Some(name.to_string());
            continue;
        }
        let Some(flag) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) else {
            bail!("unexpected argument '{arg}'");
        };
        let mut value = |flag: &str| it.next().ok_or_else(|| anyhow::anyhow!("-{flag} needs a value"));
        match flag {
            "avd" => o.avd = Some(value(flag)?),
            "list-avds" => o.list_avds = true,
            "version" => o.version = true,
            "help" | "h" => o.help = true,
            "accel-check" => o.accel_check = true,
            "accel" => {
                let v = value(flag)?;
                o.accel = match v.as_str() {
                    "auto" => None,
                    "on" => Some(true),
                    "off" => Some(false),
                    v => bail!("-accel {v}: expected on, off or auto"),
                };
                if !SOFTWARE_MODE {
                    o.ignored.push(format!("-accel {v}"));
                }
            }
            "no-accel" => {
                o.accel = Some(false);
                if !SOFTWARE_MODE {
                    o.ignored.push("-no-accel".into());
                }
            }
            "cores" => o.cores = Some(value(flag)?.parse()?),
            "memory" => o.memory_mb = Some(value(flag)?.parse()?),
            "port" => o.port = Some(value(flag)?.parse()?),
            "read-only" => o.read_only = true,
            "wipe-data" => o.wipe_data = true,
            "show-kernel" => o.show_kernel = true,
            "logcat" => o.logcat = Some(value(flag)?),
            "logcat-output" => o.logcat_output = Some(value(flag)?),
            "verbose" | "debug-init" => o.verbose = true,
            "no-boot-anim" => o.no_boot_anim = true,
            "no-window" => o.no_window = true,
            "no-audio" => o.no_audio = true,
            "allow-host-audio" => o.allow_host_audio = true,
            // `-audio BACKEND`: QEMU's audio driver; only `none` means anything here.
            "audio" => o.no_audio |= value(flag)? == "none",
            "gpu" => {
                let mode = value(flag)?;
                // Not a stock mode: the host renderer, plus the present path.
                o.metal_present = mode == "metal";
                let mode = if o.metal_present { "host".to_owned() } else { mode };
                emu_avd::gpu::parse_flag(&mode).map_err(|e| anyhow::anyhow!(e))?;
                o.gpu = Some(mode);
            }
            "netspeed" => {
                let v = value(flag)?;
                if !o.network.set_speed(&v) {
                    bail!("invalid -netspeed parameter '{v}'");
                }
            }
            "netdelay" => {
                let v = value(flag)?;
                if !o.network.set_latency(&v) {
                    bail!("invalid -netdelay parameter '{v}'");
                }
            }
            "netfast" => o.network = Default::default(),
            "disk-cache" => {
                o.disk_fsync = match value(flag)?.as_str() {
                    "fsync" | "writethrough" | "safe" => true,
                    "unsafe" | "writeback" | "none" => false,
                    other => bail!("-disk-cache takes unsafe or fsync, not '{other}'"),
                }
            }
            "create-avd" => o.create_avd = Some(value(flag)?),
            "profile" => {
                let name = value(flag)?;
                if !emu_avd::PROFILES.contains(&name.as_str()) {
                    bail!("-profile takes {}, not '{name}'", emu_avd::PROFILES.join(" or "));
                }
                o.profile = Some(name);
            }
            "clone-avd" => o.clone_avd = Some(value(flag)?),
            "from" => o.from = Some(value(flag)?),
            "delete-avd" => o.delete_avd = Some(value(flag)?),
            "sysdir" => o.sysdir = Some(value(flag)?),
            "lcd" => o.lcd = Some(parse_lcd(&value(flag)?)?),
            "provision" => o.provision = Some(value(flag)?),
            "provision-renderer" => o.provision_renderer = true,
            "no-renderer" => o.no_renderer = true,
            "no-golden" => o.no_golden = true,
            "list-images" => o.list_images = true,
            "tag" => o.tag = Some(value(flag)?),
            "abi" => o.abi = Some(value(flag)?),
            "dry-run" => o.dry_run = true,
            "camera-back" | "camera-front" => {
                let v = value(flag)?;
                if let Err(e) = emu_vmm::pipe::camera::parse_link(&v) {
                    bail!("invalid -{flag} value '{v}': {e}");
                }
                *if flag == "camera-back" { &mut o.camera_back } else { &mut o.camera_front } = Some(v);
            }
            // Everything after -qemu belongs to QEMU, which we aren't.
            "qemu" => {
                let rest: Vec<String> = it.by_ref().collect();
                o.ignored.push(format!("-qemu {}", rest.join(" ")));
            }
            f if IGNORED_WITH_VALUE.contains(&f) => {
                let v = value(f)?;
                o.ignored.push(format!("-{f} {v}"));
            }
            f => o.ignored.push(format!("-{f}")),
        }
    }
    if o.profile.is_some() && o.create_avd.is_none() {
        bail!("-profile goes with -create-avd (it shapes the AVD that makes)");
    }
    Ok(o)
}

impl Opts {
    /// What each camera links to at start: its flag, else `hw.camera.back` / `hw.camera.front`
    /// from config.ini (`config`), else none. A config value that isn't a source is reported and
    /// skipped; flags were checked when parsed.
    #[cfg_attr(
        not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))),
        allow(dead_code)
    )]
    pub fn camera_links(&self, config: impl Fn(&str) -> Option<String>) -> Vec<(Facing, Option<SourceSpec>)> {
        [(Facing::Back, &self.camera_back), (Facing::Front, &self.camera_front)]
            .into_iter()
            .map(|(facing, flag)| {
                let key = format!("hw.camera.{}", facing.dir());
                let parsed = match flag {
                    Some(value) => (value.clone(), emu_vmm::pipe::camera::parse_link(value)),
                    None => {
                        let value = config(&key).unwrap_or_default();
                        let link = config_link(&key, &value);
                        (value, link)
                    }
                };
                let (value, link) = parsed;
                let link = link.unwrap_or_else(|e| {
                    eprintln!("emu: {key}={value} isn't a camera source ({e}); the {} camera is off", facing.dir());
                    None
                });
                (facing, link)
            })
            .collect()
    }
}

/// A `hw.camera.*` value from an AVD's config.ini as a link. A real host camera is never opened
/// implicitly from a config file (that triggers the camera permission and light): `emulated`,
/// `virtualscene`, `videoplayback`, `webcam0`, `webcamN` and any other bare word are the test
/// card, and only an explicit choice (a `-camera-*` flag, the console's `camera link`, or a simee
/// source id such as `webcam:obs` in the file) links a host camera.
#[cfg_attr(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))), allow(dead_code))]
fn config_link(key: &str, value: &str) -> Result<Option<SourceSpec>, String> {
    let v = value.trim();
    if matches!(v, "" | "none" | "emulated" | "virtualscene") || v.contains(':') {
        return emu_vmm::pipe::camera::parse_link(v);
    }
    eprintln!(
        "emu: {key}={v} in config.ini isn't opened implicitly; using the test card (use -camera-back/-camera-front {v} or `camera link` for a host camera)"
    );
    emu_vmm::pipe::camera::parse_link("pattern:testcard")
}

fn parse_lcd(v: &str) -> Result<(u32, u32, u32)> {
    let (size, dpi) = v.split_once('@').unwrap_or((v, "320"));
    let (w, h) = size.split_once('x').ok_or_else(|| anyhow::anyhow!("-lcd wants WxH@DPI, got '{v}'"))?;
    Ok((w.parse()?, h.parse()?, dpi.parse()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Opts {
        parse(s.split_whitespace().map(String::from)).expect("parse")
    }

    #[test]
    fn gpu_modes_are_validated() {
        for m in ["guest", "software", "swangle", "off"] {
            assert_eq!(p(&format!("-gpu {m}")).gpu.as_deref(), Some(m));
        }
        assert!(parse("-gpu bogus".split_whitespace().map(String::from)).is_err());
        assert_eq!(p("-avd x").gpu, None);
        // `metal` is the host renderer with the present path; nothing else turns that on.
        let metal = p("-gpu metal");
        assert_eq!((metal.gpu.as_deref(), metal.metal_present), (Some("host"), true));
        assert!(!p("-gpu host").metal_present && !p("-avd x").metal_present);
        assert!(!p("-gpu metal -gpu host").metal_present, "the last -gpu wins");
    }

    #[test]
    fn accel_modes_are_stocks() {
        assert_eq!(p("-avd x").accel, None);
        assert_eq!(p("-avd x -accel auto").accel, None);
        assert_eq!(p("-avd x -accel on").accel, Some(true));
        assert_eq!(p("-avd x -accel off").accel, Some(false));
        assert_eq!(p("-avd x -no-accel -port 5570").accel, Some(false));
        assert_eq!(p("-avd x -no-accel -port 5570").port, Some(5570), "-no-accel takes no value");
        // Acted on in software mode; elsewhere `-verbose` lists it as not implemented.
        let ignored = if SOFTWARE_MODE { vec![] } else { vec!["-accel on".to_string()] };
        assert_eq!(p("-avd x -accel on").ignored, ignored);
        assert_eq!(p("-avd x -no-accel").ignored.is_empty(), SOFTWARE_MODE);
        assert!(parse("-avd x -accel hvf".split_whitespace().map(String::from)).is_err());
    }

    #[test]
    fn harness_launch_line() {
        let o = p("-avd pixel -no-window -no-audio -no-snapshot-save -gpu swiftshader_indirect -grpc 8554 -port 5570 -no-boot-anim");
        assert_eq!(o.avd.as_deref(), Some("pixel"));
        assert_eq!(o.port, Some(5570));
        assert!(o.no_window && o.no_boot_anim && o.no_audio);
        assert_eq!(o.gpu.as_deref(), Some("swiftshader_indirect"));
        assert!(o.ignored.iter().any(|i| i == "-grpc 8554"));
    }

    #[test]
    fn audio_flags() {
        let o = p("-avd pixel -allow-host-audio -port 5570");
        assert!(o.allow_host_audio && !o.no_audio);
        assert_eq!(o.port, Some(5570), "-allow-host-audio takes no value");
        assert!(p("-avd pixel -audio none").no_audio);
        assert!(!p("-avd pixel -audio coreaudio").no_audio);
        assert!(!p("-avd pixel").no_audio && !p("-avd pixel").allow_host_audio);
    }

    #[test]
    fn at_name_and_qemu_tail() {
        let o = p("@pixel -cores 2 -qemu -m 2048 -smp 2");
        assert_eq!(o.avd.as_deref(), Some("pixel"));
        assert_eq!(o.cores, Some(2));
        assert_eq!(o.ignored, vec!["-qemu -m 2048 -smp 2".to_string()]);
    }

    #[test]
    fn create_avd_flags() {
        let o = p("-create-avd small -sysdir system-images/android-36/google_apis/arm64-v8a -lcd 480x800@240 -cores 2");
        assert_eq!(o.create_avd.as_deref(), Some("small"));
        assert_eq!(o.lcd, Some((480, 800, 240)));
        assert!(parse_lcd("480").is_err());
        let o = p("-clone-avd farm-1 -from golden");
        assert_eq!((o.clone_avd.as_deref(), o.from.as_deref()), (Some("farm-1"), Some("golden")));
        assert_eq!(p("-delete-avd farm-1").delete_avd.as_deref(), Some("farm-1"));
        assert!(!o.no_golden && p("-create-avd small -sysdir x -no-golden").no_golden);
        assert!(p("-provision 36 -create-avd small -no-golden").no_golden);
    }

    #[test]
    fn profile_flag() {
        assert_eq!(p("-create-avd lite -sysdir x -profile software-lite").profile.as_deref(), Some("software-lite"));
        assert_eq!(p("-provision 36 -create-avd lite -profile software-lite").profile.as_deref(), Some("software-lite"));
        assert_eq!(p("-create-avd plain -sysdir x").profile, None);
        let err = parse("-avd pixel -profile software-lite".split(' ').map(String::from)).expect_err("no -create-avd").to_string();
        assert!(err.contains("-create-avd"), "{err}");
        let err = parse("-create-avd lite -profile fast".split(' ').map(String::from)).expect_err("unknown profile").to_string();
        assert!(err.contains("software-lite") && err.contains("fast"), "{err}");
    }

    #[test]
    fn provision_flags() {
        let o =
            p("-provision 36 -tag google_apis_playstore -abi arm64-v8a -create-avd play36 -lcd 480x800@240 -cores 2 -memory 1536 -dry-run");
        assert_eq!(o.provision.as_deref(), Some("36"));
        assert_eq!(o.tag.as_deref(), Some("google_apis_playstore"));
        assert_eq!(o.abi.as_deref(), Some("arm64-v8a"));
        assert_eq!(o.create_avd.as_deref(), Some("play36"));
        assert_eq!((o.lcd, o.cores, o.memory_mb), (Some((480, 800, 240)), Some(2), Some(1536)));
        assert!(o.dry_run && !o.list_images && o.ignored.is_empty());
        assert!(!o.no_renderer && !o.provision_renderer);

        let o = p("-provision 36 -no-renderer");
        assert!(o.no_renderer && !o.provision_renderer && o.ignored.is_empty());
        let o = p("-provision-renderer -dry-run");
        assert!(o.provision_renderer && o.dry_run && o.provision.is_none() && o.ignored.is_empty());

        let o = p("-list-images -tag android-tv");
        assert!(o.list_images && o.provision.is_none());
        assert_eq!(o.tag.as_deref(), Some("android-tv"));
        assert!(parse(["-provision".to_string()]).is_err());
        assert!(parse(["-list-images".to_string(), "-tag".to_string()]).is_err());
    }

    #[test]
    fn network_flags_set_the_initial_conditions() {
        let o = p("-avd pixel -netspeed edge -netdelay 20:80 -port 5570");
        assert_eq!((o.network.upload_bps, o.network.download_bps), (473600.0, 473600.0));
        assert_eq!((o.network.min_latency_ms, o.network.max_latency_ms), (20, 80));
        assert!(o.ignored.is_empty());
        assert!(!p("-avd pixel -netspeed edge -netfast").network.is_shaped());
        assert!(!p("-avd pixel").network.is_shaped());
        assert!(parse(["-netspeed".to_string(), "warp".to_string()]).is_err());
        assert!(parse(["-netdelay".to_string(), "slow".to_string()]).is_err());
    }

    #[test]
    fn camera_flags_link_sources() {
        let o = p("-avd pixel -camera-back emulated -camera-front qr:hello -port 5570");
        assert_eq!((o.camera_back.as_deref(), o.camera_front.as_deref()), (Some("emulated"), Some("qr:hello")));
        assert!(o.ignored.is_empty() && o.port == Some(5570));
        let config = |k: &str| (k == "hw.camera.front").then(|| "none".to_string());
        let links = o.camera_links(config);
        assert_eq!(links[0], (Facing::Back, Some(SourceSpec::parse("pattern:testcard").expect("card"))));
        assert_eq!(links[1], (Facing::Front, Some(SourceSpec::Qr("hello".into()))), "the flag wins over config.ini");

        // No flags: config.ini's stock values; one that isn't a source is off.
        let config = |k: &str| Some(if k == "hw.camera.back" { "virtualscene" } else { "bogus:x" }.to_string());
        let links = p("-avd pixel").camera_links(config);
        assert!(links[0].1.is_some() && links[1].1.is_none());
        assert_eq!(p("-avd pixel").camera_links(|_| None), vec![(Facing::Back, None), (Facing::Front, None)]);
        assert!(p("-camera-back webcam0").camera_back.is_some(), "a host camera, resolved when the guest opens it");
        assert!(parse(["-camera-back".to_string(), "bogus:x".to_string()]).is_err());
    }

    #[test]
    fn config_ini_never_opens_a_host_camera() {
        let card = Some(SourceSpec::parse("pattern:testcard").expect("card"));
        let cfg = |v: &'static str| move |_: &str| Some(v.to_string());
        for v in ["emulated", "virtualscene", "webcam0", "webcam3", "videoplayback", "somename"] {
            let links = p("-avd pixel").camera_links(cfg(v));
            assert_eq!((links[0].1.clone(), links[1].1.clone()), (card.clone(), card.clone()), "{v}");
        }
        assert_eq!(p("-avd pixel").camera_links(cfg("none")), vec![(Facing::Back, None), (Facing::Front, None)]);
        // A simee source id in config.ini is explicit.
        let links = p("-avd pixel").camera_links(cfg("webcam:obs"));
        assert_eq!(links[0].1, Some(SourceSpec::Webcam("obs".into())));
        // So is a flag, for its own camera only.
        let links = p("-avd pixel -camera-back webcam0").camera_links(cfg("webcam1"));
        assert_eq!(links[0].1, Some(SourceSpec::Webcam("webcam0".into())));
        assert_eq!(links[1].1, card, "the front camera's config.ini value stays the test card");
    }

    #[test]
    fn missing_value_is_an_error() {
        assert!(parse(["-port".to_string()]).is_err());
        assert!(parse(["stray".to_string()]).is_err());
    }
}
