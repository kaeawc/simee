//! Android Virtual Device discovery and configuration.
//!
//! Reads the same files the SDK tools write (`~/.android/avd/<name>.ini`,
//! `<name>.avd/config.ini`, the system image directory) so existing AVDs boot unchanged.

mod bootconfig;
pub mod bundle;
mod cache;
#[cfg(unix)]
pub mod clone;
#[cfg(not(unix))]
#[path = "clone_copy.rs"]
pub mod clone;
pub mod golden;
pub mod gpu;
mod ini;
pub mod sdk_device;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use bootconfig::BootConfig;
pub use ini::parse_ini;
use sim_core::form_factor::{ANDROID_GLASSES_SCREEN, Density, FormFactor, Profile, Shape, android_profile};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown AVD '{0}' (looked in {1})")]
    UnknownAvd(String, PathBuf),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Bundle(#[from] sim_bundle::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// Where the SDK and AVDs live, resolved like the SDK tools do.
#[derive(Clone, Debug)]
pub struct Env {
    pub sdk_root: Option<PathBuf>,
    pub avd_home: PathBuf,
}

/// Where Android Studio puts the SDK, relative to the home directory.
pub const DEFAULT_SDK: &str = if cfg!(target_os = "macos") { "Library/Android/sdk" } else { "Android/Sdk" };

impl Env {
    /// `ANDROID_SDK_ROOT` / `ANDROID_HOME` (then the platform default) for the SDK;
    /// `ANDROID_AVD_HOME`, then `ANDROID_EMULATOR_HOME/avd`, then `ANDROID_USER_HOME/avd`,
    /// then `~/.android/avd` for AVDs.
    pub fn from_env() -> Env {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var("HOME").unwrap_or_else(|| PathBuf::from("."));
        let sdk_root = var("ANDROID_SDK_ROOT").or_else(|| var("ANDROID_HOME")).or_else(|| {
            let p = home.join(DEFAULT_SDK);
            p.is_dir().then_some(p)
        });
        let avd_home = var("ANDROID_AVD_HOME")
            .or_else(|| var("ANDROID_EMULATOR_HOME").map(|p| p.join("avd")))
            .or_else(|| var("ANDROID_USER_HOME").map(|p| p.join("avd")))
            .unwrap_or_else(|| home.join(".android/avd"));
        Env { sdk_root, avd_home }
    }

    /// AVD names, sorted, as `emulator -list-avds` prints them.
    pub fn list_avds(&self) -> Vec<String> {
        cache::listing(&self.avd_home, || self.read_avds())
    }

    /// The names [`Env::list_avds`] lists, read from the home, and whether that answer can be
    /// remembered: not when any read failed (running out of file handles must not become "no
    /// AVDs" until the directory changes), nor when a pointer is a link, because the directory's
    /// stamp doesn't change when a link's target comes or goes.
    fn read_avds(&self) -> (Vec<String>, bool) {
        let mut cacheable = true;
        let mut names = Vec::new();
        match std::fs::read_dir(&self.avd_home) {
            Err(_) => cacheable = false,
            Ok(entries) => {
                for e in entries {
                    let Ok(e) = e else {
                        cacheable = false;
                        continue;
                    };
                    let Ok(name) = e.file_name().into_string() else { continue };
                    let Some(stem) = name.strip_suffix(".ini") else { continue };
                    // The entry's own type (no stat); only a link needs following.
                    let file = match e.file_type() {
                        Ok(t) if t.is_file() => true,
                        Ok(t) if t.is_dir() => false,
                        Ok(_) => {
                            cacheable = false;
                            e.path().is_file()
                        }
                        Err(_) => {
                            cacheable = false;
                            e.path().is_file()
                        }
                    };
                    if file {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        (names, cacheable)
    }

    /// Whether [`Env::list_avds`] lists `name`, from one stat of its pointer `<name>.ini` instead
    /// of listing (and stating) the whole home: a UDID that names no AVD, what every iOS call
    /// asks, costs a failed stat. Exact case, as the listing compares: on a case-insensitive
    /// filesystem (APFS) `Pixel.ini` opens `pixel.ini`, so a found pointer must be stored under
    /// that very name; one that isn't (another case, a link to another name) is settled by the
    /// listing.
    pub fn has_avd(&self, name: &str) -> bool {
        if name.is_empty() || name.contains('/') {
            return false;
        }
        let file = format!("{name}.ini");
        let pointer = self.avd_home.join(&file);
        if !pointer.is_file() {
            return false;
        }
        match std::fs::canonicalize(&pointer) {
            Ok(stored) if stored.file_name() == Some(file.as_ref()) => true,
            _ => self.list_avds().iter().any(|n| n == name),
        }
    }

    pub fn load(&self, name: &str) -> Result<Avd> {
        self.load_in(name, self.content_dir(name)?)
    }

    /// [`Env::load`] for an AVD whose directory the caller resolved ([`Env::content_dir`]).
    pub fn load_in(&self, name: &str, content_dir: PathBuf) -> Result<Avd> {
        let path = content_dir.join("config.ini");
        let config = cache::ini(&path)?.ok_or_else(|| Error::Io(path, std::io::ErrorKind::NotFound.into()))?;
        Avd::new(name, content_dir, config, self.sdk_root.as_deref())
    }

    /// AVD `name`'s directory: its pointer's `path=` (or `path.rel=`), else `<name>.avd`.
    pub fn content_dir(&self, name: &str) -> Result<PathBuf> {
        let pointer = self.avd_home.join(format!("{name}.ini"));
        Ok(if let Some(p) = cache::ini(&pointer)? {
            p.get("path")
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .or_else(|| p.get("path.rel").map(|r| self.avd_home.parent().unwrap_or(&self.avd_home).join(r)))
                .unwrap_or_else(|| self.avd_home.join(format!("{name}.avd")))
        } else {
            let d = self.avd_home.join(format!("{name}.avd"));
            if !d.is_dir() {
                return Err(Error::UnknownAvd(name.to_string(), self.avd_home.clone()));
            }
            d
        })
    }
}

/// Whether `name` is an AVD name `create_avd` accepts: letters, digits, `.`, `_`, `-`, and not
/// `.` or `..` (so `<avd home>/<name>.avd` and `<name>.ini` stay inside the AVD home).
fn valid_avd_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Console port claims: simee holds `<dir>/<port>.lock` locked for its process's life, taken
/// before it probes or binds the port (`$HOME/Library/Caches/simee/ports`).
pub fn port_lock_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/simee/ports"))
}

/// Whether some process holds console port `port`'s claim in `dir`. The probe takes the lock
/// for an instant when it's free, so an emu claiming that port at the same moment may see it
/// taken: launchers that pass `-port` retry on another pair when emu exits with [`PORT_TAKEN`].
pub fn port_claimed(dir: &Path, port: u16) -> bool {
    std::fs::File::open(dir.join(format!("{port}.lock"))).is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}

/// What emu says (in its exit error) when `-port N`'s claim is held by another instance.
pub const PORT_TAKEN: &str = "is taken by another simee instance";

/// Lock files the stock emulator keeps in an AVD directory while it runs it: `multiinstance.lock`
/// (a file lock) and `hardware-qemu.ini.lock` (holds its PID). Also simee's own.
fn held_lock(dir: &Path) -> Option<PathBuf> {
    let candidates = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lock"))
        .chain(std::iter::once(state_dir(dir).join("instance.lock")));
    for path in candidates {
        let Ok(f) = std::fs::File::open(&path) else { continue };
        if matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
            return Some(path);
        }
        // Held by us now if the lock was free: Windows' locks are mandatory, so reading the file
        // below by another handle fails until it is let go (unix's are advisory).
        let _ = f.unlock();
        // A PID lock (stock's filelock: the PID, then NUL) whose process is alive. A stale one
        // from a crash names a dead PID (or, rarely, a reused one: refusing then is safe).
        let pid = std::fs::read(&path).ok().and_then(|b| {
            let digits: String = b.iter().take_while(|c| c.is_ascii_digit()).map(|&c| c as char).collect();
            digits.parse::<i32>().ok()
        });
        if pid.is_some_and(pid_alive) {
            return Some(path);
        }
    }
    None
}

/// A resolved AVD: its directory, config and system image.
#[derive(Clone, Debug)]
pub struct Avd {
    pub name: String,
    pub dir: PathBuf,
    /// `config.ini`'s keys, shared with the remembered parse (`Arc::make_mut` to edit a copy).
    pub config: Arc<BTreeMap<String, String>>,
    pub sysdir: PathBuf,
    /// The SDK the image came from (holds the gfxstream renderer in `emulator/lib64` or
    /// `simee/renderer/<revision>/lib64`).
    pub sdk_root: Option<PathBuf>,
}

impl Avd {
    fn new(name: &str, dir: PathBuf, config: Arc<BTreeMap<String, String>>, sdk_root: Option<&Path>) -> Result<Avd> {
        let sysdir = (1..=3)
            .filter_map(|i| config.get(&format!("image.sysdir.{i}")))
            .map(|rel| match sdk_root {
                Some(root) if Path::new(rel).is_relative() => root.join(rel),
                _ => PathBuf::from(rel),
            })
            .find(|p| p.join("kernel-ranchu").is_file() || p.join("system.img").is_file())
            .ok_or_else(|| Error::Config(format!("{name}: no system image found (image.sysdir.N in config.ini, SDK root {sdk_root:?})")))?;
        Ok(Avd { name: name.to_string(), dir, config, sysdir, sdk_root: sdk_root.map(Path::to_path_buf) })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.config.get(key).map(String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some("yes" | "true"))
    }

    /// Whether the guest is x86_64: `abi.type` or `hw.cpu.arch` in `config.ini`, else the system
    /// image's directory (`.../x86_64`). x86_64 guests run only in software mode (#55), on stock's
    /// x86 board; everything else is arm64.
    pub fn is_x86_64(&self) -> bool {
        match self.get("abi.type").or_else(|| self.get("hw.cpu.arch")) {
            Some(abi) => abi == "x86_64",
            None => self.sysdir.file_name().is_some_and(|n| n == "x86_64"),
        }
    }

    pub fn cpus(&self) -> u32 {
        self.get("hw.cpu.ncore").and_then(|v| v.parse().ok()).unwrap_or(2).clamp(1, 16)
    }

    /// Guest RAM in bytes (`hw.ramSize`, which accepts `2048`, `2048M` or `2G`).
    ///
    /// Below 1 GB the stock launcher's 512 MB of padding is added: current Wear images are
    /// configured with 512 MB and the OOM killer takes `system_server` and adbd without it.
    /// Guest RAM is committed lazily, so the padding costs host memory only if it's used.
    pub fn ram_bytes(&self) -> u64 {
        let mb = self.get("hw.ramSize").and_then(parse_size_mb).unwrap_or(2048);
        let mb = if mb < 1024 { mb + 512 } else { mb };
        mb * 1024 * 1024
    }

    /// `disk.dataPartition.size`, a disk size: `6G`, `2048M`, or plain bytes (`6442450944`, as
    /// Android Studio and the stock emulator write it).
    pub fn data_partition_bytes(&self) -> u64 {
        self.get("disk.dataPartition.size").and_then(parse_disk_size).unwrap_or(6 << 30)
    }

    pub fn lcd(&self) -> (u32, u32, u32) {
        let n = |k: &str, d: u32| self.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        (n("hw.lcd.width", 1080), n("hw.lcd.height", 2400), n("hw.lcd.density", 420))
    }

    /// The display's refresh rate in Hz (`hw.lcd.vsync`, as stock reads it), 60 when unset or
    /// outside 1..=240. The guest's composer paces vsync by it. Under software mode's TCG, every
    /// vsync callback is translated work: 30 Hz settled 0.12 cores lower than 60 (docs/benchmarks.md).
    pub fn vsync_hz(&self) -> u32 {
        self.get("hw.lcd.vsync").and_then(|v| v.parse().ok()).filter(|hz| (1..=240).contains(hz)).unwrap_or(60)
    }

    pub fn has_sdcard(&self) -> bool {
        self.flag("hw.sdCard")
    }

    pub fn kernel(&self) -> PathBuf {
        self.sysdir.join("kernel-ranchu")
    }

    pub fn ramdisk(&self) -> PathBuf {
        self.sysdir.join("ramdisk.img")
    }

    pub fn system_image(&self) -> PathBuf {
        self.sysdir.join("system.img")
    }

    pub fn vendor_image(&self) -> PathBuf {
        self.sysdir.join("vendor.img")
    }

    /// What the device is (phone, tablet, foldable, Wear OS watch, TV, Automotive head unit) and
    /// its screen, shape, fold and inputs, inferred from `config.ini`. The system image tag is
    /// read from the image's `source.properties` when the config doesn't name it.
    pub fn profile(&self) -> Profile {
        let image_tag = || {
            let props = std::fs::read_to_string(self.sysdir.join("source.properties")).ok()?;
            parse_ini(&props).get("SystemImage.TagId").cloned()
        };
        android_profile(|k| match (self.get(k), k) {
            (Some(v), _) => Some(v.to_owned()),
            (None, "tag.id") => image_tag(),
            (None, _) => None,
        })
    }

    /// Whether the device has hardware Back/Home keys (`hw.mainKeys`, stock's default yes). The
    /// guest hides its navigation bar when it does (`qemu.hw.mainkeys=1`).
    pub fn main_keys(&self) -> bool {
        self.get("hw.mainKeys").is_none_or(|v| matches!(v.trim(), "yes" | "true"))
    }

    /// The properties stock forwards to the guest at boot from `boot.prop`: the AVD's own, else
    /// the system image's (avdmanager writes `ro.emulator.circular=true` for round watches).
    pub fn boot_properties(&self) -> Vec<(String, String)> {
        [self.dir.join("boot.prop"), self.sysdir.join("boot.prop")]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse_properties(&text))
            .unwrap_or_default()
    }

    /// The properties the stock host hands the guest's `qemu-props` for this AVD: `boot.prop`'s
    /// lines, then the LCD density and whether there are hardware keys (set last, as in stock).
    pub fn qemu_props(&self) -> Vec<(String, String)> {
        let mut props = self.boot_properties();
        props.push(("qemu.sf.lcd_density".to_owned(), self.lcd().2.to_string()));
        props.push(("qemu.hw.mainkeys".to_owned(), if self.main_keys() { "1" } else { "0" }.to_owned()));
        props
    }

    /// Whether the screen is round: `ro.emulator.circular` in `boot.prop` (what stock checks;
    /// `1`, `y`, `on`, `yes`, `true` and their prefixes count), or `hw.lcd.circular`.
    /// The touchpad's width and height in units (`hw.touchpad0`: the SDK's display glasses).
    pub fn touchpad(&self) -> Option<(u32, u32)> {
        let flag = self.get("hw.touchpad0").is_some_and(|v| matches!(v.trim(), "yes" | "true"));
        let num = |k: &str| self.get(k).and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0);
        flag.then(|| Some((num("hw.touchpad0.width")?, num("hw.touchpad0.height")?))).flatten()
    }

    /// Whether the screen takes touch: everything but `hw.screen=no-touch` (the SDK's TVs).
    pub fn touch_screen(&self) -> bool {
        self.get("hw.screen").map(str::trim) != Some("no-touch")
    }

    pub fn circular(&self) -> bool {
        let truthy = |v: &str| ["1", "y", "on", "yes", "true"].iter().any(|t| v.starts_with(t));
        self.boot_properties().iter().any(|(k, v)| k == "ro.emulator.circular" && truthy(v)) || self.flag("hw.lcd.circular")
    }

    /// The sensors the guest's HAL is told about (`list-sensors`), decided like stock: by the
    /// `hw.sensor*` flags, plus heart rate and wrist tilt on Wear OS at API 28 and later.
    pub fn sensor_mask(&self) -> u32 {
        sim_core::goldfish::sensor_mask(|k| self.get(k).map(str::to_owned), self.profile().kind.form_factor, self.api_level())
    }

    /// The system image's `build.prop` (SDK images ship it beside `system.img`); empty if absent.
    pub fn build_props(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(self.sysdir.join("build.prop")).map(|t| parse_ini(&t)).unwrap_or_default()
    }

    /// Whether the image is Android Automotive, decided as stock does: from `build.prop`. An
    /// image without one falls back to its tag (`android-automotive*`).
    pub fn is_car(&self) -> bool {
        let props = self.build_props();
        if props.is_empty() { self.image_tags().contains("automotive") } else { is_car_build(&props) }
    }

    /// Whether the image's `advancedFeatures.ini` turns `name` on (`VirtioSndCard = on`); a
    /// feature it doesn't list is off, as stock reads it. None when the image has no such file.
    pub fn image_feature(&self, name: &str) -> Option<bool> {
        let text = std::fs::read_to_string(self.sysdir.join("advancedFeatures.ini")).ok()?;
        Some(text.lines().any(|l| l.split_once('=').is_some_and(|(k, v)| k.trim() == name && v.trim().eq_ignore_ascii_case("on"))))
    }

    /// The system image tags (`tag.id`, `tag.ids` in `config.ini`, else the image's
    /// `source.properties`), lowercase and comma-separated.
    pub fn image_tags(&self) -> String {
        let configured = [self.get("tag.id"), self.get("tag.ids")].into_iter().flatten().collect::<Vec<_>>().join(",");
        let tags = if configured.is_empty() {
            let props = std::fs::read_to_string(self.sysdir.join("source.properties")).unwrap_or_default();
            parse_ini(&props).get("SystemImage.TagId").cloned().unwrap_or_default()
        } else {
            configured
        };
        tags.to_ascii_lowercase()
    }

    /// API level from the system image's `source.properties`, if present.
    pub fn api_level(&self) -> Option<u32> {
        let props = std::fs::read_to_string(self.sysdir.join("source.properties")).ok()?;
        parse_ini(&props).get("AndroidVersion.ApiLevel")?.parse().ok()
    }

    /// Extra kernel command line the image ships (`kernel_cmdline.txt`).
    pub fn image_cmdline(&self) -> String {
        std::fs::read_to_string(self.sysdir.join("kernel_cmdline.txt")).map(|s| s.trim().to_string()).unwrap_or_default()
    }

    /// `androidboot.vbmeta.*` parameters from `VerifiedBootParams.textproto`.
    pub fn vbmeta_params(&self) -> Vec<(String, String)> {
        let Ok(text) = std::fs::read_to_string(self.sysdir.join("VerifiedBootParams.textproto")) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("param:"))
            .filter_map(|v| {
                let v = v.trim().trim_matches('"');
                let (k, val) = v.split_once('=')?;
                Some((k.to_string(), val.to_string()))
            })
            .collect()
    }
}

/// What `create_avd` writes. Defaults favour cheap, fast boots: a small screen boots markedly
/// faster than a large one, and more than 2 GB of RAM makes cold boots slower, not faster.
#[derive(Clone, Debug)]
pub struct NewAvd {
    pub name: String,
    /// Relative to the SDK root, e.g. `system-images/android-36/google_apis/arm64-v8a/`.
    pub sysdir: String,
    pub lcd: (u32, u32, u32),
    pub ram_mb: u64,
    pub cores: u32,
    pub data_gb: u32,
    /// Hardware Back/Home keys (`hw.mainKeys`): the guest then shows no navigation bar.
    pub main_keys: bool,
    /// A D-pad (`hw.dPad`).
    pub dpad: bool,
    /// More `config.ini` lines (a foldable's hinge sensor, for one).
    pub extra: Vec<(String, String)>,
    /// `<avd>/boot.prop` lines, forwarded to the guest at boot (a round watch's
    /// `ro.emulator.circular`). None writes no file.
    pub boot_props: Vec<(String, String)>,
}

impl NewAvd {
    /// A phone: on-screen navigation, no D-pad, a hardware keyboard.
    pub fn new(name: &str, sysdir: &str) -> NewAvd {
        NewAvd {
            name: name.to_string(),
            sysdir: sysdir.trim_end_matches('/').to_string() + "/",
            lcd: (720, 1280, 320),
            ram_mb: 2048,
            cores: 4,
            data_gb: 4,
            main_keys: false,
            dpad: false,
            extra: Vec::new(),
            boot_props: Vec::new(),
        }
    }

    /// An AVD shaped like its system image's own kind, from the tag in `sysdir`
    /// (`system-images/android-34/android-wear/arm64-v8a`): a round watch for Wear OS, a TV with a
    /// D-pad, a headset for Android XR, a phone for mobile images.
    pub fn for_image(name: &str, sysdir: &str) -> NewAvd {
        let tag = sysdir.trim_matches('/').split('/').nth(2).unwrap_or_default().to_owned();
        let profile = sim_core::form_factor::android_profile(|k| (k == "tag.id").then(|| tag.clone()));
        NewAvd::for_profile(name, sysdir, &profile.kind.image_profile(&tag))
    }

    /// An AVD shaped like `profile`: its screen, and the hardware the SDK's device profiles give
    /// its form factor.
    pub fn for_profile(name: &str, sysdir: &str, profile: &Profile) -> NewAvd {
        let mut spec = NewAvd::new(name, sysdir);
        if let Density::Dpi(dpi) = profile.density {
            spec.lcd = (profile.width, profile.height, dpi);
        }
        let kv = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        // The SDK device definition the form factor follows; its hardware keys go in as
        // avdmanager writes them.
        let device = match profile.kind.form_factor {
            FormFactor::Watch => Some(sdk_device::WEAR_LARGE_ROUND),
            FormFactor::Tv => Some(sdk_device::TV_1080P),
            FormFactor::Auto => Some(sdk_device::AUTOMOTIVE_1024P_LANDSCAPE),
            FormFactor::Vision if spec.lcd == ANDROID_GLASSES_SCREEN => Some(sdk_device::AI_GLASSES),
            FormFactor::Vision => Some(sdk_device::XR_HEADSET),
            FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable => None,
        };
        if let Some(device) = device {
            spec.ram_mb = device.ram_mb;
            spec.main_keys = device.hard_buttons;
            spec.dpad = device.nav == sdk_device::Nav::Dpad;
            spec.extra = device.config();
        }
        match profile.kind.form_factor {
            // The foldable definition the screen is (the Pixel Fold unless it is the
            // dual-screen's), with the hinge keys avdmanager writes for it.
            FormFactor::Foldable => {
                if let Some(fold) = profile.fold {
                    let mut device = sim_core::fold::devices()
                        .into_iter()
                        .find(|d| (d.lcd.0, d.lcd.1) == (profile.width, profile.height))
                        .unwrap_or_else(sim_core::fold::pixel_fold);
                    device.hinge.default = fold.angle.clamp(device.hinge.range.0, device.hinge.range.1);
                    spec.extra = device.config();
                }
            }
            // Wear OS: 512 MB, booted with stock's 512 MB of padding (see `Avd::ram_bytes`);
            // avdmanager marks a round screen in boot.prop.
            FormFactor::Watch => {
                if profile.shape == Shape::Round {
                    spec.boot_props.push(kv("ro.emulator.circular", "true"));
                }
            }
            // Automotive images' own hardware defaults (device/generic/car/common/config.ini)
            // also turn off the sensors the SDK definition doesn't mention.
            FormFactor::Auto => spec.extra.extend([kv("hw.sensors.temperature", "no"), kv("hw.sensors.humidity", "no")]),
            // Android XR: both SDK devices are landscape.
            FormFactor::Vision => spec.extra.push(kv("hw.initialOrientation", "landscape")),
            FormFactor::Phone | FormFactor::Tablet | FormFactor::Tv => {}
        }
        spec
    }
}

impl Env {
    /// Creates an AVD the way the SDK's avdmanager lays it out (`<name>.ini` pointer and
    /// `<name>.avd/config.ini`), with no Java involved. Refuses to overwrite an existing AVD.
    /// It writes no `fastboot.*` keys: emu has no snapshots and ignores them, and the stock
    /// emulator then quick-boots the AVD as it would one avdmanager made (with
    /// `fastboot.forceColdBoot=yes` its quick boots took 15 s instead of 2.8 s).
    pub fn create_avd(&self, spec: &NewAvd) -> Result<PathBuf> {
        if !valid_avd_name(&spec.name) {
            return Err(Error::Config(format!("invalid AVD name '{}' (letters, digits, . _ -)", spec.name)));
        }
        let dir = self.avd_home.join(format!("{}.avd", spec.name));
        let pointer = self.avd_home.join(format!("{}.ini", spec.name));
        if dir.exists() || pointer.exists() {
            return Err(Error::Config(format!("AVD '{}' already exists", spec.name)));
        }
        let parts: Vec<&str> = spec.sysdir.trim_end_matches('/').split('/').collect();
        let (target, tag, abi) = match parts.as_slice() {
            [.., target, tag, abi] => (*target, *tag, *abi),
            _ => return Err(Error::Config(format!("system image dir '{}' should end in <target>/<tag>/<abi>", spec.sysdir))),
        };
        let arch = if abi == "arm64-v8a" { "arm64" } else { abi };
        let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        let pointer_body = format!("avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/{}.avd\ntarget={target}\n", dir.display(), spec.name);
        std::fs::write(&pointer, pointer_body).map_err(|e| io(&pointer, e))?;
        let (w, h, dpi) = spec.lcd;
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        let config = format!(
            "AvdId={name}\navd.ini.displayname={name}\nabi.type={abi}\nhw.cpu.arch={arch}\nhw.cpu.ncore={cores}\n\
             hw.ramSize={ram}\nhw.lcd.width={w}\nhw.lcd.height={h}\nhw.lcd.density={dpi}\nhw.gpu.enabled=yes\n\
             hw.gpu.mode=auto\nhw.keyboard=yes\nhw.mainKeys={main_keys}\nhw.dPad={dpad}\nhw.trackBall=no\n\
             hw.sdCard=no\nimage.sysdir.1={sysdir}\ntag.id={tag}\ndisk.dataPartition.size={data}G\n",
            name = spec.name,
            cores = spec.cores,
            ram = spec.ram_mb,
            main_keys = yes_no(spec.main_keys),
            dpad = yes_no(spec.dpad),
            sysdir = spec.sysdir,
            data = spec.data_gb,
        );
        let lines = |kv: &[(String, String)]| kv.iter().map(|(k, v)| format!("{k}={v}\n")).collect::<String>();
        // `extra` keys replace the defaults above rather than repeat them.
        let base: String =
            config.lines().filter(|l| !spec.extra.iter().any(|(k, _)| l.split('=').next() == Some(k))).map(|l| format!("{l}\n")).collect();
        let cfg = dir.join("config.ini");
        std::fs::write(&cfg, base + &lines(&spec.extra)).map_err(|e| io(&cfg, e))?;
        if !spec.boot_props.is_empty() {
            let boot = dir.join("boot.prop");
            std::fs::write(&boot, lines(&spec.boot_props)).map_err(|e| io(&boot, e))?;
        }
        Ok(dir)
    }

    /// Creates AVD `name` as a copy of `golden` whose simee disks (`<golden>/simee/`: userdata,
    /// cache, metadata) are copy-on-write clones ([`clone::clone_file`]: `clonefile` on APFS, a `FICLONE`
    /// reflink on Linux file systems that have them, else a copy that keeps the image's holes), so a
    /// clone takes no space until either side writes, and it boots warm
    /// (no first-boot formatting, encryption or dexopt) from the state the golden was settled in.
    /// The clone runs persistent, with its own state from then on.
    ///
    /// Only the golden's `config.ini`, `boot.prop` and `simee/` files are read; its stock
    /// emulator files (qcow2 overlays, snapshots) are never touched or copied. The golden must be
    /// stopped (its instance lock is held for the copy) and must have booted under simee once.
    ///
    /// What tells clones apart: the AVD name (`androidboot.qemu.avd_name`, the console's
    /// `avd name`) and the adb serial `emulator-<console port>`, which is per launch. Like the
    /// stock emulator's `-read-only` instances, clones keep the golden's guest identity
    /// (`ro.serialno`, Settings' android_id and per-app SSAIDs live in the cloned userdata).
    pub fn clone_avd(&self, golden: &str, name: &str) -> Result<PathBuf> {
        let src = self.load(golden)?;
        let src_disks = state_dir(&src.dir);
        let userdata = src_disks.join("userdata.img");
        if !userdata.is_file() {
            return Err(Error::Config(format!("{golden} has no simee user data yet (boot it once with emu, then stop it)")));
        }
        let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
        // Held while copying, so the golden can't start (or is refused if it's running).
        let lock_path = src_disks.join("instance.lock");
        let lock = std::fs::File::create(&lock_path).map_err(|e| io(&lock_path, e))?;
        if lock.try_lock().is_err() {
            return Err(Error::Config(format!("{golden} is running; stop it before cloning it")));
        }
        let sysdir = src.get("image.sysdir.1").unwrap_or_default().to_owned();
        self.create_avd(&NewAvd::new(name, &sysdir))?;
        let dir = self.avd_home.join(format!("{name}.avd"));
        let made = (|| {
            // The golden's config, renamed: same hardware and image.
            let config: String = read(&src.dir.join("config.ini"))?
                .lines()
                .map(|l| match l.split_once('=').map(|(k, _)| k.trim()) {
                    Some("AvdId") => format!("AvdId={name}\n"),
                    Some("avd.ini.displayname") => format!("avd.ini.displayname={name}\n"),
                    _ => format!("{l}\n"),
                })
                .collect();
            let cfg = dir.join("config.ini");
            std::fs::write(&cfg, config).map_err(|e| io(&cfg, e))?;
            let boot = src.dir.join("boot.prop");
            if boot.is_file() {
                clone::clone_file(&boot, &dir.join("boot.prop"))?;
            }
            let disks = state_dir(&dir);
            std::fs::create_dir_all(&disks).map_err(|e| io(&disks, e))?;
            for f in CLONED_DISKS {
                let from = src_disks.join(f);
                if from.is_file() {
                    let to = disks.join(f);
                    // The key is what a guest boots with: it lands whole or not at all (#431).
                    if f == "encryptionkey.img" {
                        clone::clone_file_atomic(&from, &to)?;
                    } else {
                        clone::clone_file(&from, &to)?;
                    }
                }
            }
            Ok(())
        })();
        drop(lock);
        if let Err(e) = made {
            let _ = self.delete_avd(name);
            return Err(e);
        }
        Ok(dir)
    }

    /// Deletes AVD `name`: its directory (where its `<name>.ini` points, as [`Env::load`] finds
    /// it) and that pointer, nothing else. Refused while any emulator runs it, simee or stock:
    /// one advertised in [`running_dir`] for it, or a lock file in its directory that is held
    /// (simee's `simee/instance.lock`, stock's `multiinstance.lock`, `hardware-qemu.ini.lock`).
    pub fn delete_avd(&self, name: &str) -> Result<()> {
        self.delete_avd_in(name, running_dir().as_deref())
    }

    /// [`Env::delete_avd`] with running emulators advertised in `running`.
    pub fn delete_avd_in(&self, name: &str, running: Option<&Path>) -> Result<()> {
        if !valid_avd_name(name) {
            return Err(Error::Config(format!("invalid AVD name '{name}' (letters, digits, . _ -)")));
        }
        let pointer = self.avd_home.join(format!("{name}.ini"));
        let dir = self.content_dir(name)?;
        if dir.exists() && !dir.join("config.ini").is_file() {
            return Err(Error::Config(format!("{} has no config.ini; not deleting a directory that isn't an AVD", dir.display())));
        }
        let real = dir.canonicalize().ok();
        let advertised = running.map(running_instances_in).unwrap_or_default().into_iter().find(|r| {
            r.avd_name == name || r.avd_dir.as_deref().is_some_and(|d| d == dir || (real.is_some() && d.canonicalize().ok() == real))
        });
        if let Some(r) = advertised {
            return Err(Error::Config(format!("{name} is running ({}, PID {}); stop it before deleting it", r.serial(), r.pid)));
        }
        if let Some(lock) = held_lock(&dir) {
            return Err(Error::Config(format!("{name} is running ({} is held); stop it before deleting it", lock.display())));
        }
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| Error::Io(dir.clone(), e))?;
        }
        if pointer.exists() {
            std::fs::remove_file(&pointer).map_err(|e| Error::Io(pointer.clone(), e))?;
        }
        Ok(())
    }
}

/// emu's own state under an AVD or SDK directory: `<avd>/simee/` (disks, logs, display socket,
/// frame rings) and `<sdk>/simee/renderer/`.
pub const STATE_DIR: &str = "simee";

/// `<avd>/simee`: an AVD's emu state.
pub fn state_dir(avd_dir: &Path) -> PathBuf {
    avd_dir.join(STATE_DIR)
}

/// The simee disks a clone gets from its golden AVD (`<avd>/simee/`): user data, the cache
/// partition and the metadata partition that holds user data's encryption key, so the three stay
/// consistent.
pub const CLONED_DISKS: [&str; 3] = ["userdata.img", "cache.img", "encryptionkey.img"];

/// Where running emulators advertise themselves (`pid_<pid>.ini`), as the SDK tools look for them.
pub fn running_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/TemporaryItems/avd/running"))
}

/// A running emulator, from its advertisement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub pid: i32,
    pub avd_name: String,
    pub console_port: u16,
    pub adb_port: u16,
    pub avd_dir: Option<PathBuf>,
    /// simee's `emu` runs it (it advertised itself with `emulator=simee`); `false` for the stock
    /// emulator, found by [`Env::stock_instances_among_with`].
    pub simee: bool,
}

impl Running {
    /// Which emulator runs it: `simee` or `stock`.
    pub fn emulator(&self) -> &'static str {
        if self.simee { "simee" } else { "stock" }
    }

    /// The adb serial, `emulator-<console port>`.
    pub fn serial(&self) -> String {
        format!("emulator-{}", self.console_port)
    }

    /// Parses `pid_<pid>.ini` named `file_name`.
    pub fn parse(file_name: &str, text: &str) -> Option<Running> {
        let pid = file_name.strip_prefix("pid_")?.strip_suffix(".ini")?.parse().ok()?;
        let kv = parse_ini(text);
        let console_port: u16 = kv.get("port.serial")?.parse().ok()?;
        Some(Running {
            pid,
            avd_name: kv.get("avd.name")?.clone(),
            console_port,
            adb_port: kv.get("port.adb").and_then(|p| p.parse().ok()).unwrap_or(console_port + 1),
            avd_dir: kv.get("avd.dir").map(PathBuf::from),
            simee: kv.get("emulator").is_some_and(|e| e == "simee"),
        })
    }

    /// Whether its process still exists.
    pub fn alive(&self) -> bool {
        pid_alive(self.pid)
    }
}

/// Whether process `pid` exists (`kill(pid, 0)`; a process of another user counts).
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid).is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

/// Whether process `pid` exists and hasn't exited.
#[cfg(windows)]
fn pid_alive(pid: i32) -> bool {
    u32::try_from(pid).is_ok_and(emu_hvf::host::process_alive)
}

/// What the host says about a process, for [`Env::stock_instances_among_with`].
pub struct Probe<'a> {
    /// The process's command line; `None` when it has gone.
    pub args: &'a dyn Fn(i32) -> Option<String>,
    /// The TCP ports the process listens on.
    pub listening: &'a dyn Fn(i32) -> Vec<u16>,
}

/// The console and adb ports a stock emulator was started with: `-port N` (adb is `N + 1`) or
/// `-ports N,M` (console, adb) in its command line (the launcher execs the qemu binary with the
/// same arguments).
pub fn ports_from_args(args: &str) -> Option<(u16, u16)> {
    let words: Vec<&str> = args.split_whitespace().collect();
    words.windows(2).find_map(|w| match w[0] {
        "-port" => w[1].parse::<u16>().ok().and_then(|c| Some((c, c.checked_add(1)?))),
        "-ports" => {
            let (c, a) = w[1].split_once(',')?;
            Some((c.parse().ok()?, a.parse().ok()?))
        }
        _ => None,
    })
}

/// Whether `args` is a qemu command line started for AVD `name`: the exact pair `-avd <name>`
/// (adjacent words), not just any word equal to the name.
pub fn qemu_args_run_avd(args: &str, name: &str) -> bool {
    args.contains("qemu") && args.split_whitespace().collect::<Vec<_>>().windows(2).any(|w| w[0] == "-avd" && w[1] == name)
}

/// Whether process `pid` is, right now, a qemu running AVD `name` (asks `ps`).
pub fn process_runs_avd(pid: i32, name: &str) -> bool {
    host_probe_args(pid).is_some_and(|a| qemu_args_run_avd(&a, name))
}

/// The console port among the listening `ports` of an emulator started without `-port`: the
/// lowest even port in the emulator range (5554..=5682) whose adb port (next) listens too.
pub fn console_port_from_listening(ports: &[u16]) -> Option<u16> {
    ports.iter().copied().filter(|p| (5554..=5682).contains(p) && p % 2 == 0 && ports.contains(&(p + 1))).min()
}

fn host_probe_args(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

impl Env {
    /// AVDs the stock emulator is running. This emulator doesn't write the `pid_<pid>.ini` that
    /// [`running_instances_in`] reads (checked on emulator 36.x, macOS: `<tmp>/avd/running` stays
    /// empty); it holds `<avd>/multiinstance.lock` and writes the qemu PID into
    /// `<avd>/hardware-qemu.ini.lock`. The PID's command line has `-avd <name>` and usually
    /// `-port N`; without it the console port is the emulator-range port the process listens on.
    /// Instances in `skip` (by AVD name) are left out, so a simee `emu` is never listed twice.
    ///
    /// Only the AVDs `names` are looked at (each one's lock), and `probe` is asked only about a
    /// process whose lock names a live PID.
    pub fn stock_instances_among_with(&self, names: &[String], skip: &[Running], probe: &Probe) -> Vec<Running> {
        let avds: Vec<(String, PathBuf)> = names.iter().filter_map(|name| Some((name.clone(), self.content_dir(name).ok()?))).collect();
        Self::stock_instances_in_with(avds.iter().map(|(n, d)| (n.as_str(), d.as_path())), skip, probe)
    }

    /// [`Env::stock_instances_among_with`] for AVDs whose directories the caller resolved
    /// ([`Env::content_dir`]), so listing and looking at them resolves each once.
    pub fn stock_instances_in_with<'a>(
        avds: impl IntoIterator<Item = (&'a str, &'a Path)>,
        skip: &[Running],
        probe: &Probe,
    ) -> Vec<Running> {
        avds.into_iter()
            .filter(|(name, _)| !skip.iter().any(|r| r.avd_name == *name))
            .filter_map(|(name, dir)| {
                let bytes = std::fs::read(dir.join("hardware-qemu.ini.lock")).ok()?;
                let digits: String = bytes.iter().take_while(|c| c.is_ascii_digit()).map(|&c| c as char).collect();
                let pid: i32 = digits.parse().ok()?;
                let r = Running {
                    pid,
                    avd_name: name.to_owned(),
                    console_port: 0,
                    adb_port: 0,
                    avd_dir: Some(dir.to_path_buf()),
                    simee: false,
                };
                if !r.alive() {
                    return None;
                }
                // A reused PID is some other program: it must be a qemu started for this AVD.
                let args = (probe.args)(pid)?;
                if !qemu_args_run_avd(&args, &r.avd_name) {
                    return None;
                }
                let (console_port, adb_port) =
                    ports_from_args(&args).or_else(|| console_port_from_listening(&(probe.listening)(pid)).map(|c| (c, c + 1)))?;
                Some(Running { console_port, adb_port, ..r })
            })
            .collect()
    }
}

/// Every advertised emulator whose process is still alive.
pub fn running_instances() -> Vec<Running> {
    running_dir().map(|d| running_instances_in(&d)).unwrap_or_default()
}

/// Every emulator advertised in `dir` whose process is still alive.
pub fn running_instances_in(dir: &Path) -> Vec<Running> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Running::parse(e.file_name().to_str()?, &std::fs::read_to_string(e.path()).ok()?))
        .filter(Running::alive)
        .collect()
}

/// Parses an Android property file (`name=value` lines; blank lines, `#` comments and lines
/// without `=` skipped), in order.
pub fn parse_properties(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// Parses `2048`, `2048M`, `2048MB`, `2G`, `2GB` into megabytes.
pub fn parse_size_mb(v: &str) -> Option<u64> {
    let v = v.trim();
    let upper = v.to_ascii_uppercase();
    let (num, mult) = if let Some(n) = upper.strip_suffix("GB").or_else(|| upper.strip_suffix('G')) {
        (n.to_string(), 1024)
    } else if let Some(n) = upper.strip_suffix("MB").or_else(|| upper.strip_suffix('M')) {
        (n.to_string(), 1)
    } else if let Some(n) = upper.strip_suffix("KB").or_else(|| upper.strip_suffix('K')) {
        return n.trim().parse::<u64>().ok().map(|k| k.div_ceil(1024));
    } else {
        (upper.clone(), 1)
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

/// Parses a disk size the way stock's `diskSize` properties take it into bytes: a `K`, `M` or
/// `G` suffix (with or without `B`, any case), or plain bytes. Unlike `hw.ramSize`, whose plain
/// number is megabytes.
pub fn parse_disk_size(v: &str) -> Option<u64> {
    let upper = v.trim().to_ascii_uppercase();
    let upper = upper.strip_suffix('B').unwrap_or(&upper);
    let (num, shift) = match upper.as_bytes().last()? {
        b'G' => (&upper[..upper.len() - 1], 30),
        b'M' => (&upper[..upper.len() - 1], 20),
        b'K' => (&upper[..upper.len() - 1], 10),
        _ => (upper, 0),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(1 << shift)
}

/// Stock's car-image test (`qemu-setup.cpp`, `avd/util.c`): the product name, the system
/// product name or the build flavor contains `car` (`sdk_car_arm64`, `gcar_emu_arm64-userdebug`).
/// Stock registers the `qemud:car` service only for these images.
pub fn is_car_build(props: &BTreeMap<String, String>) -> bool {
    ["ro.product.name", "ro.product.system.name", "ro.build.flavor"].iter().any(|k| props.get(*k).is_some_and(|v| v.contains("car")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_ports_come_from_args_then_listening_sockets() {
        let args = "/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless -avd lane -port 5750 -no-window";
        assert_eq!(ports_from_args(args), Some((5750, 5751)));
        assert_eq!(ports_from_args("qemu -avd lane -ports 5600,5601"), Some((5600, 5601)));
        assert_eq!(ports_from_args("qemu -avd lane -ports 5600,5700"), Some((5600, 5700)), "the adb port is the second value");
        assert_eq!(ports_from_args("qemu -avd lane -ports 5600"), None);
        assert_eq!(ports_from_args("qemu -avd lane -no-window"), None);
        // The AVD name must follow `-avd`, not merely appear.
        assert!(qemu_args_run_avd(args, "lane"));
        assert!(!qemu_args_run_avd("qemu -avd other -writable-system lane", "lane"));
        assert!(!qemu_args_run_avd("qemu -avd lane2 -no-window", "lane"));
        assert!(!qemu_args_run_avd("vim -avd lane", "lane"), "not a qemu");
        assert_eq!(console_port_from_listening(&[51686, 5555, 5554, 5555, 8080]), Some(5554));
        assert_eq!(console_port_from_listening(&[5554]), None, "an adb port must listen too");
    }

    #[test]
    fn stock_instances_are_read_from_lock_files_and_checked_for_life() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-stock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let env = Env { sdk_root: None, avd_home: tmp.clone() };
        let me = std::process::id();
        // Fixtures as stock writes them: the PID, then a NUL, in `hardware-qemu.ini.lock`.
        for (name, lock) in [
            ("live", Some(format!("{me}\0"))),
            ("noport", Some(format!("{me}\0"))),
            ("dead", Some("999999999\0".to_owned())),
            ("reused", Some(format!("{me}\0"))),
            ("idle", None),
            ("simee", Some(format!("{me}\0"))),
        ] {
            let dir = tmp.join(format!("{name}.avd"));
            std::fs::create_dir_all(&dir).expect("avd dir");
            std::fs::write(tmp.join(format!("{name}.ini")), format!("path={}\n", dir.display())).expect("pointer");
            if let Some(lock) = lock {
                std::fs::write(dir.join("hardware-qemu.ini.lock"), lock).expect("lock");
            }
        }
        let args = |_pid: i32| Some("/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless -avd X -port 5750".to_owned());
        let probe = Probe { args: &|pid| args(pid).map(|a| a.replace(" X ", " live ")), listening: &|_| vec![] };
        let skip = [Running { pid: 1, avd_name: "simee".into(), console_port: 5600, adb_port: 5601, avd_dir: None, simee: true }];
        let found = env.stock_instances_among_with(&env.list_avds(), &skip, &probe);
        let names: Vec<&str> = found.iter().map(|r| r.avd_name.as_str()).collect();
        // `noport`/`reused` lack `-avd <their name>` in the fixture args; `dead` has no process;
        // `idle` has no lock; `simee` is already listed by its advertisement.
        assert_eq!(names, ["live"]);
        let r = &found[0];
        assert_eq!((r.console_port, r.adb_port, r.pid, r.simee, r.emulator()), (5750, 5751, me as i32, false, "stock"));
        assert_eq!(r.serial(), "emulator-5750");
        // Started without `-port`: the listening sockets say.
        let probe = Probe {
            args: &|_| Some("qemu-system-aarch64-headless -avd noport -no-window".to_owned()),
            listening: &|_| vec![5557, 5556, 51686],
        };
        let found = env.stock_instances_among_with(&env.list_avds(), &skip, &probe);
        let noport = found.iter().find(|r| r.avd_name == "noport").expect("noport");
        assert_eq!(noport.console_port, 5556);
        // A PID that now belongs to another program is not an emulator.
        let probe = Probe { args: &|_| Some("/usr/bin/vim notes.txt".to_owned()), listening: &|_| vec![] };
        assert!(env.stock_instances_among_with(&env.list_avds(), &skip, &probe).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn one_avd_is_probed_alone_and_ports_flags_give_the_adb_port() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-one-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let env = Env { sdk_root: None, avd_home: tmp.clone() };
        for name in ["a", "b"] {
            let dir = tmp.join(format!("{name}.avd"));
            std::fs::create_dir_all(&dir).expect("dir");
            std::fs::write(tmp.join(format!("{name}.ini")), format!("path={}\n", dir.display())).expect("ini");
            std::fs::write(dir.join("hardware-qemu.ini.lock"), format!("{}\0", std::process::id())).expect("lock");
        }
        let asked = std::cell::Cell::new(0);
        let probe = Probe {
            args: &|_| {
                asked.set(asked.get() + 1);
                Some("qemu-system-aarch64 -avd a -ports 5600,5700".to_owned())
            },
            listening: &|_| panic!("ports are in the args: no lsof"),
        };
        let found = env.stock_instances_among_with(&["a".to_owned()], &[], &probe);
        assert_eq!(asked.get(), 1, "only the named AVD's process is asked about");
        assert_eq!((found[0].console_port, found[0].adb_port), (5600, 5700));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn car_images_are_found_from_build_prop() {
        let props = |text: &str| parse_ini(text);
        assert!(is_car_build(&props("ro.product.name=sdk_car_arm64\n")));
        assert!(is_car_build(&props("ro.product.system.name=gcar_emu_arm64\n")));
        assert!(is_car_build(&props("ro.build.flavor=sdk_car_md_arm64-userdebug\n")));
        assert!(!is_car_build(&props("ro.product.name=sdk_gphone64_arm64\nro.build.flavor=sdk_gphone64_arm64-userdebug\n")));
        assert!(!is_car_build(&BTreeMap::new()));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size_mb("2048"), Some(2048));
        assert_eq!(parse_size_mb("2G"), Some(2048));
        assert_eq!(parse_size_mb("10G"), Some(10240));
        assert_eq!(parse_size_mb("66MB"), Some(66));
        assert_eq!(parse_size_mb("512M"), Some(512));
        assert_eq!(parse_size_mb("bogus"), None);
        // Disk sizes: a plain number is bytes there (a 6 GB partition was read as 6 PB).
        assert_eq!(parse_disk_size("6442450944"), Some(6 << 30));
        assert_eq!(parse_disk_size("6g"), Some(6 << 30));
        assert_eq!(parse_disk_size("10G"), Some(10 << 30));
        assert_eq!(parse_disk_size("800MB"), Some(800 << 20));
        assert_eq!(parse_disk_size("66m"), Some(66 << 20));
        assert_eq!(parse_disk_size("512k"), Some(512 << 10));
        assert_eq!(parse_disk_size(""), None);
        assert_eq!(parse_disk_size("bogus"), None);
    }

    #[test]
    fn running_advertisements_parse() {
        let text = "port.serial=5554\nport.adb=5555\navd.name=pixel\navd.dir=/a/pixel.avd\nemulator=simee\n";
        let r = Running::parse("pid_42.ini", text).expect("parsed");
        assert_eq!((r.pid, r.avd_name.as_str(), r.console_port, r.adb_port, r.simee), (42, "pixel", 5554, 5555, true));
        assert_eq!(r.serial(), "emulator-5554");
        assert_eq!(r.avd_dir.as_deref(), Some(Path::new("/a/pixel.avd")));
        let stock = Running::parse("pid_7.ini", "port.serial=5556\navd.name=other\n").expect("stock");
        assert_eq!((stock.adb_port, stock.simee), (5557, false));
        assert_eq!(Running::parse("notes.txt", text), None);
        assert_eq!(Running::parse("pid_1.ini", "avd.name=x\n"), None);
        let me = Running { pid: std::process::id() as i32, ..r };
        assert!(me.alive());
    }

    // Unix mode bits decide what can be listed or searched.
    #[cfg(unix)]
    #[test]
    fn has_avd_stats_one_pointer_instead_of_listing_the_home() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("emu-avd-has-{}", std::process::id()));
        std::fs::create_dir_all(home.join("pixel.avd")).expect("mkdir");
        std::fs::write(home.join("pixel.ini"), "path.rel=avd/pixel.avd\n").expect("write");
        std::fs::create_dir_all(home.join("dir.ini")).expect("a directory, not a pointer");
        let env = Env { sdk_root: None, avd_home: home.clone() };
        let names = env.list_avds();
        assert_eq!(names, ["pixel"]);
        for name in ["pixel", "Pixel", "pix", "", "dir", "../avd/pixel", "pixel/", "/pixel", "C45634B5-6C9D-4C35-89BE-586942279DFA"] {
            assert_eq!(env.has_avd(name), names.iter().any(|n| n == name), "{name:?} as list_avds says");
        }
        // A home that can be searched but not read: a listing finds nothing, one stat still works.
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o300)).expect("chmod");
        // Root (or a filesystem that ignores modes) can still list it: the permission isn't enforced.
        let enforced = std::fs::read_dir(&home).is_err();
        let unlisted = env.list_avds();
        let (found, not) = (env.has_avd("pixel"), env.has_avd("pix"));
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("chmod back");
        if !enforced {
            println!("SKIPPED: has_avd without listing: mode 0300 does not block listing here (running as root?)");
            let _ = std::fs::remove_dir_all(&home);
            return;
        }
        assert!(unlisted.is_empty(), "the home can't be listed: {unlisted:?}");
        assert!(found, "found by its pointer alone");
        assert!(!not);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn a_listing_with_a_link_or_a_failed_read_is_not_cacheable() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-readavds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("home");
        let env = Env { sdk_root: None, avd_home: tmp.clone() };
        std::fs::write(tmp.join("a.ini"), "").expect("a");
        std::fs::create_dir(tmp.join("d.ini")).expect("a directory named like a pointer");
        assert_eq!(env.read_avds(), (vec!["a".to_owned()], true));
        // A link: its target coming or going doesn't touch the home's stamp.
        std::fs::write(tmp.join("target"), "").expect("target");
        std::os::unix::fs::symlink(tmp.join("target"), tmp.join("b.ini")).expect("link");
        assert_eq!(env.read_avds(), (vec!["a".to_owned(), "b".to_owned()], false));
        std::fs::remove_file(tmp.join("target")).expect("dangling");
        assert_eq!(env.read_avds(), (vec!["a".to_owned()], false), "a dangling link isn't an AVD");
        // A home that can't be read is no AVDs this time, not for good.
        let missing = Env { sdk_root: None, avd_home: tmp.join("missing") };
        assert_eq!(missing.read_avds(), (Vec::new(), false));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn loads_avd_from_dir() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-test-{}", std::process::id()));
        let avd_home = tmp.join("avd");
        let sdk = tmp.join("sdk");
        let sys = sdk.join("system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(avd_home.join("pixel.avd")).expect("mkdir");
        std::fs::create_dir_all(&sys).expect("mkdir");
        std::fs::write(sys.join("kernel-ranchu"), b"k").expect("write");
        std::fs::write(sys.join("source.properties"), "AndroidVersion.ApiLevel=36\n").expect("write");
        std::fs::write(
            avd_home.join("pixel.avd/config.ini"),
            "hw.cpu.ncore=4\nhw.ramSize=2G\nimage.sysdir.1=system-images/android-36/google_apis/arm64-v8a/\nhw.lcd.width=1080\n",
        )
        .expect("write");
        std::fs::write(avd_home.join("pixel.ini"), format!("path={}\n", avd_home.join("pixel.avd").display())).expect("write");
        let env = Env { sdk_root: Some(sdk.clone()), avd_home };
        assert_eq!(env.list_avds(), vec!["pixel".to_string()]);
        let avd = env.load("pixel").expect("load");
        assert_eq!(avd.cpus(), 4);
        assert_eq!(avd.ram_bytes(), 2 << 30);
        let mut small = avd.clone();
        Arc::make_mut(&mut small.config).insert("hw.ramSize".into(), "512".into());
        assert_eq!(small.ram_bytes(), 1 << 30, "stock pads small devices by 512 MB");
        assert_eq!(avd.api_level(), Some(36));
        assert!(!avd.is_x86_64());
        assert_eq!(avd.vsync_hz(), 60, "stock's default refresh rate");
        let mut slow = avd.clone();
        Arc::make_mut(&mut slow.config).insert("hw.lcd.vsync".into(), "30".into());
        assert_eq!(slow.vsync_hz(), 30);
        for bad in ["0", "1000", "sixty"] {
            Arc::make_mut(&mut slow.config).insert("hw.lcd.vsync".into(), bad.into());
            assert_eq!(slow.vsync_hz(), 60, "{bad} falls back to 60");
        }
        let mut x86 = avd.clone();
        Arc::make_mut(&mut x86.config).insert("abi.type".into(), "x86_64".into());
        assert!(x86.is_x86_64(), "abi.type says so");
        Arc::make_mut(&mut x86.config).clear();
        x86.sysdir = sdk.join("system-images/android-36/default/x86_64");
        assert!(x86.is_x86_64(), "an AVD with neither key takes its image's ABI");
        assert!(env.load("nope").is_err());

        // A created AVD loads back with the requested hardware, and can't be created twice.
        let mut spec = NewAvd::new("small", "system-images/android-36/google_apis/arm64-v8a");
        spec.lcd = (480, 800, 240);
        env.create_avd(&spec).expect("create");
        let made = env.load("small").expect("load created");
        assert_eq!((made.lcd(), made.cpus(), made.ram_bytes()), ((480, 800, 240), 4, 2 << 30));
        assert!(env.create_avd(&spec).is_err());
        assert!(env.create_avd(&NewAvd::new("../evil", "a/b/c")).is_err());
        assert_eq!(made.profile().kind.form_factor, FormFactor::Phone);
        // No `fastboot.*` keys: emu has no snapshots and ignores them, and the stock emulator
        // keeps its own default (quick boot) on an AVD emu made, as on one avdmanager made.
        assert!(made.config.keys().all(|k| !k.starts_with("fastboot.")), "{:?}", made.config.keys().collect::<Vec<_>>());

        // Form factors come back from what was written: a foldable's hinge, a Wear image's tag.
        let fold = sim_core::form_factor::Kind::new(sim_core::platform::Platform::Android, FormFactor::Foldable).expect("fold").profile();
        env.create_avd(&NewAvd::for_profile("fold", "system-images/android-36/google_apis/arm64-v8a", &fold)).expect("create fold");
        let made = env.load("fold").expect("load fold");
        assert_eq!(made.lcd(), (2208, 1840, 420));
        let p = made.profile();
        assert_eq!((p.kind.form_factor, p.fold.map(|f| f.posture())), (FormFactor::Foldable, Some(sim_core::form_factor::Posture::Opened)));
        // The Pixel Fold's keys as avdmanager writes them, so stock folds it to its outer screen.
        assert_eq!((made.get("hw.device.name"), made.get("hw.sensor.hinge.type")), (Some("pixel_fold"), Some("1")));
        assert_eq!((made.get("hw.displayRegion.0.1.width"), made.get("hw.displayRegion.0.1.height")), (Some("1080"), Some("2092")));
        let duo = fold.clone().with_screen(2784, 1800, sim_core::form_factor::Density::Dpi(401));
        env.create_avd(&NewAvd::for_profile("duo", "system-images/android-36/google_apis/arm64-v8a", &duo)).expect("create duo");
        let duo = env.load("duo").expect("load duo");
        assert_eq!((duo.get("hw.device.name"), duo.get("hw.sensor.hinge.ranges")), (Some("surface_duo"), Some("0-360")));
        assert_eq!(duo.profile().kind.form_factor, FormFactor::Foldable);
        let wear_sys = sdk.join("system-images/android-34/android-wear/arm64-v8a");
        std::fs::create_dir_all(&wear_sys).expect("mkdir");
        std::fs::write(wear_sys.join("system.img"), b"s").expect("write");
        std::fs::write(wear_sys.join("source.properties"), "SystemImage.TagId=android-wear\n").expect("write");
        let mut watch = NewAvd::new("watch", "system-images/android-34/android-wear/arm64-v8a");
        watch.lcd = (454, 454, 320);
        env.create_avd(&watch).expect("create watch");
        let w = env.load("watch").expect("load watch").profile();
        assert_eq!((w.kind.sub_platform.as_str(), w.shape), ("wearos", sim_core::form_factor::Shape::Round));
        assert!(!env.load("watch").expect("load watch").is_car());

        // Automotive: build.prop decides; without one, the image tag does.
        let car_sys = sdk.join("system-images/android-34/android-automotive/arm64-v8a");
        std::fs::create_dir_all(&car_sys).expect("mkdir");
        std::fs::write(car_sys.join("system.img"), b"s").expect("write");
        std::fs::write(car_sys.join("source.properties"), "SystemImage.TagId=android-automotive\n").expect("write");
        env.create_avd(&NewAvd::new("car", "system-images/android-34/android-automotive/arm64-v8a")).expect("create car");
        let car = env.load("car").expect("load car");
        assert!(car.image_tags().contains("android-automotive"));
        assert!(car.is_car(), "from the tag");
        std::fs::write(car_sys.join("build.prop"), "ro.product.name=sdk_gphone64_arm64\n").expect("write");
        assert!(!car.is_car(), "build.prop wins");
        std::fs::write(car_sys.join("build.prop"), "# begin build properties\nro.build.flavor=sdk_car_arm64-userdebug\n").expect("write");
        assert!(car.is_car());

        // Android XR: the headset's screen, 4 GiB and landscape; glasses get theirs and 2 GiB.
        let xr_sys = sdk.join("system-images/android-34/google-xr/arm64-v8a");
        std::fs::create_dir_all(&xr_sys).expect("mkdir");
        std::fs::write(xr_sys.join("system.img"), b"s").expect("write");
        let xr = sim_core::form_factor::Kind::new(sim_core::platform::Platform::Android, FormFactor::Vision).expect("Android XR");
        let spec = NewAvd::for_profile("headset", "system-images/android-34/google-xr/arm64-v8a", &xr.profile());
        assert_eq!((spec.lcd, spec.ram_mb), ((2560, 2558, 240), 4096));
        env.create_avd(&spec).expect("create headset");
        let made = env.load("headset").expect("load headset");
        assert_eq!((made.get("hw.initialOrientation"), made.ram_bytes()), (Some("landscape"), 4 << 30));
        assert_eq!(made.profile().kind.label(), "Android XR · Vision");
        let glasses = NewAvd::for_profile("glasses", "system-images/android-36/ai-glasses/arm64-v8a", &xr.image_profile("ai-glasses"));
        assert_eq!((glasses.lcd, glasses.ram_mb), ((450, 450, 160), 2048));
        assert_eq!(made.touchpad(), None);
        let glasses_sys = sdk.join("system-images/android-36/ai-glasses/arm64-v8a");
        std::fs::create_dir_all(&glasses_sys).expect("mkdir");
        std::fs::write(glasses_sys.join("system.img"), b"s").expect("write");
        env.create_avd(&glasses).expect("create glasses");
        assert_eq!(env.load("glasses").expect("load glasses").touchpad(), Some((1542, 297)), "the temple touchpad");
        assert_eq!(made.image_feature("VirtioDualModeMouse"), None, "no advancedFeatures.ini");
        std::fs::write(xr_sys.join("advancedFeatures.ini"), "GuestAngle = on\nVirtioDualModeMouse = on\nXrModeUI=off\n").expect("write");
        assert_eq!(
            ["VirtioDualModeMouse", "XrModeUI", "VirtioMouse"].map(|f| made.image_feature(f)),
            [Some(true), Some(false), Some(false)]
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn clones_copy_the_goldens_simee_disks_and_nothing_else() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-clone-{}", std::process::id()));
        let sdk = tmp.join("sdk");
        let sys = sdk.join("system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sys).expect("mkdir");
        std::fs::write(sys.join("kernel-ranchu"), b"k").expect("write");
        let env = Env { sdk_root: Some(sdk), avd_home: tmp.join("avd") };
        let mut spec = NewAvd::new("golden", "system-images/android-36/google_apis/arm64-v8a");
        spec.lcd = (480, 800, 240);
        spec.boot_props = vec![("qemu.hw.mainkeys".into(), "1".into())];
        let gdir = env.create_avd(&spec).expect("create golden");
        assert!(env.clone_avd("golden", "c1").is_err(), "a golden that never booted under simee has no user data");
        assert!(!env.avd_home.join("c1.avd").exists() && !env.avd_home.join("c1.ini").exists(), "nothing left behind");

        std::fs::create_dir_all(state_dir(&gdir)).expect("mkdir");
        for (f, body) in [("userdata.img", "data"), ("cache.img", "cache"), ("encryptionkey.img", "key"), ("console.log", "log")] {
            std::fs::write(state_dir(&gdir).join(f), body).expect("write");
        }
        std::fs::write(gdir.join("userdata-qemu.img.qcow2"), b"stock").expect("stock overlay");
        let cdir = env.clone_avd("golden", "c1").expect("clone");
        let c = env.load("c1").expect("load clone");
        assert_eq!((c.lcd(), c.get("AvdId"), c.get("avd.ini.displayname")), ((480, 800, 240), Some("c1"), Some("c1")));
        assert_eq!(c.boot_properties(), vec![("qemu.hw.mainkeys".to_string(), "1".to_string())]);
        for (f, body) in [("userdata.img", "data"), ("cache.img", "cache"), ("encryptionkey.img", "key")] {
            assert_eq!(std::fs::read_to_string(state_dir(&cdir).join(f)).expect("cloned"), body);
        }
        assert!(!state_dir(&cdir).join("console.log").exists() && !cdir.join("userdata-qemu.img.qcow2").exists(), "only the disks");
        // Copy-on-write: the clone's writes don't reach the golden.
        std::fs::write(state_dir(&cdir).join("userdata.img"), "changed").expect("write");
        assert_eq!(std::fs::read_to_string(state_dir(&gdir).join("userdata.img")).expect("golden"), "data");
        assert!(env.clone_avd("golden", "c1").is_err(), "names are unique");

        // A running golden (its instance lock held) is refused, and so is deleting a running clone.
        let held = std::fs::File::create(state_dir(&gdir).join("instance.lock")).expect("lock");
        held.lock().expect("lock");
        assert!(env.clone_avd("golden", "c2").expect_err("running").to_string().contains("running"));
        assert!(!env.avd_home.join("c2.avd").exists());
        drop(held);
        let held = std::fs::File::create(state_dir(&cdir).join("instance.lock")).expect("lock");
        held.lock().expect("lock");
        assert!(env.delete_avd("c1").is_err());
        drop(held);
        env.delete_avd("c1").expect("delete");
        assert!(!cdir.exists() && !env.avd_home.join("c1.ini").exists());
        assert!(env.delete_avd("c1").is_err(), "already gone");
        assert!(state_dir(&gdir).join("userdata.img").is_file(), "the golden is untouched");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn delete_avd_follows_the_pointer_and_refuses_running_ones() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let sdk = tmp.join("sdk");
        let sys = sdk.join("system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sys).expect("mkdir");
        std::fs::write(sys.join("kernel-ranchu"), b"k").expect("write");
        let env = Env { sdk_root: Some(sdk), avd_home: tmp.join("avd") };
        let running = tmp.join("running");
        std::fs::create_dir_all(&running).expect("mkdir");
        let make = |name: &str| env.create_avd(&NewAvd::new(name, "system-images/android-36/google_apis/arm64-v8a")).expect("create");

        // Names are validated as create_avd does: nothing outside the AVD home is reached.
        std::fs::create_dir_all(tmp.join("outside.avd")).expect("mkdir");
        std::fs::write(tmp.join("outside.avd/config.ini"), "").expect("write");
        std::fs::write(tmp.join("outside.ini"), "path=x\n").expect("write");
        for bad in ["../outside", "a/b", "..", ""] {
            assert!(env.delete_avd_in(bad, Some(&running)).expect_err(bad).to_string().contains("invalid AVD name"), "{bad}");
        }
        assert!(tmp.join("outside.avd/config.ini").is_file() && tmp.join("outside.ini").is_file());

        // The pointer's path= is where the AVD lives: that directory goes, and the pointer.
        let moved = make("moved");
        let elsewhere = tmp.join("store/moved.avd");
        std::fs::create_dir_all(tmp.join("store")).expect("mkdir");
        std::fs::rename(&moved, &elsewhere).expect("move");
        std::fs::write(env.avd_home.join("moved.ini"), format!("path={}\n", elsewhere.display())).expect("pointer");
        std::fs::create_dir_all(env.avd_home.join("moved.avd")).expect("decoy");
        env.delete_avd_in("moved", Some(&running)).expect("delete");
        assert!(!elsewhere.exists() && !env.avd_home.join("moved.ini").exists());
        assert!(env.avd_home.join("moved.avd").is_dir(), "only the directory the pointer names");

        // A held stock lock refuses (multiinstance.lock is a file lock), and so does a live PID
        // in hardware-qemu.ini.lock; a stale PID doesn't.
        let dir = make("stock");
        let held = std::fs::File::create(dir.join("multiinstance.lock")).expect("lock");
        held.lock().expect("lock");
        assert!(env.delete_avd_in("stock", Some(&running)).expect_err("held").to_string().contains("multiinstance.lock"));
        drop(held);
        std::fs::write(dir.join("hardware-qemu.ini.lock"), format!("{}\0", std::process::id())).expect("pid lock");
        assert!(env.delete_avd_in("stock", Some(&running)).expect_err("live pid").to_string().contains("hardware-qemu.ini.lock"));
        std::fs::write(dir.join("hardware-qemu.ini.lock"), b"999999999\0").expect("stale pid lock");
        // An advertised instance of it (stock writes the same running file) refuses too.
        std::fs::write(running.join(format!("pid_{}.ini", std::process::id())), "port.serial=5600\navd.name=stock\n").expect("ad");
        assert!(env.delete_avd_in("stock", Some(&running)).expect_err("advertised").to_string().contains("emulator-5600"));
        std::fs::remove_file(running.join(format!("pid_{}.ini", std::process::id()))).expect("rm ad");
        env.delete_avd_in("stock", Some(&running)).expect("stopped: deleted");
        assert!(!dir.exists() && !env.avd_home.join("stock.ini").exists());

        // A pointer naming a directory that isn't an AVD is not deleted.
        std::fs::create_dir_all(tmp.join("notavd")).expect("mkdir");
        std::fs::write(env.avd_home.join("odd.ini"), format!("path={}\n", tmp.join("notavd").display())).expect("pointer");
        assert!(env.delete_avd_in("odd", Some(&running)).is_err());
        assert!(tmp.join("notavd").is_dir());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn port_claims_are_seen_while_held() {
        let dir = std::env::temp_dir().join(format!("emu-avd-ports-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(!port_claimed(&dir, 5600), "no lock file");
        let f = std::fs::File::create(dir.join("5600.lock")).expect("lock");
        assert!(!port_claimed(&dir, 5600), "a file nobody holds");
        f.lock().expect("lock");
        assert!(port_claimed(&dir, 5600));
        drop(f);
        assert!(!port_claimed(&dir, 5600), "released");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn new_avds_take_their_image_form_factor() {
        let shape = |sysdir: &str| {
            let s = NewAvd::for_image("x", sysdir);
            (s.lcd, s.ram_mb, s.main_keys, s.dpad)
        };
        assert_eq!(shape("system-images/android-36/google_apis/arm64-v8a"), ((720, 1280, 320), 2048, false, false));
        assert_eq!(shape("system-images/android-34/android-wear/arm64-v8a/"), ((454, 454, 320), 512, true, false));
        assert_eq!(shape("system-images/android-34/android-tv/arm64-v8a"), ((1920, 1080, 320), 2048, true, true));
        assert_eq!(shape("system-images/android-34-ext9/android-automotive/arm64-v8a"), ((1024, 768, 160), 3686, false, false));
        assert_eq!(shape("system-images/android-34/google-xr/arm64-v8a"), ((2560, 2558, 240), 4096, false, false));
        assert_eq!(shape("system-images/android-36/ai-glasses/arm64-v8a"), ((450, 450, 160), 2048, false, false));
        let wear = NewAvd::for_image("w", "system-images/android-34/android-wear/arm64-v8a");
        assert_eq!(wear.boot_props, [("ro.emulator.circular".to_owned(), "true".to_owned())]);
        let get = |spec: &NewAvd, key: &str| spec.extra.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let headset = NewAvd::for_image("h", "system-images/android-34/google-xr/arm64-v8a");
        assert_eq!(get(&headset, "hw.trackBall").as_deref(), Some("yes"), "xr_headset_device navigates by trackball");
        let glasses = NewAvd::for_image("g", "system-images/android-36/ai-glasses/arm64-v8a");
        assert_eq!(get(&glasses, "hw.device.name").as_deref(), Some("ai_glasses_device"));
        assert_eq!((get(&glasses, "hw.touchpad0").as_deref(), get(&glasses, "hw.gps").as_deref()), (Some("yes"), Some("no")));
    }

    #[test]
    fn properties_parse_in_order() {
        let text = "# avdmanager\nro.emulator.circular = true\n\n  qemu.x=a=b\nnot a property\n=v\n";
        let expect = [("ro.emulator.circular", "true"), ("qemu.x", "a=b")].map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(parse_properties(text), expect);
    }

    /// Creates each form factor's AVD from its profile and loads it back.
    #[test]
    fn new_avds_get_their_form_factors_hardware() {
        use sim_core::form_factor::Kind;
        use sim_core::platform::Platform;
        let tmp = std::env::temp_dir().join(format!("emu-avd-kinds-{}", std::process::id()));
        let sdk = tmp.join("sdk");
        let env = Env { sdk_root: Some(sdk.clone()), avd_home: tmp.join("avd") };
        let image = |tag: &str, api: u32| {
            let rel = format!("system-images/android-{api}/{tag}/arm64-v8a");
            let dir = sdk.join(&rel);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("system.img"), b"s").expect("write");
            std::fs::write(dir.join("source.properties"), format!("AndroidVersion.ApiLevel={api}\n")).expect("write");
            rel
        };
        let create = |name: &str, form_factor: FormFactor, sysdir: &str| {
            let profile = Kind::new(Platform::Android, form_factor).expect("kind").profile();
            env.create_avd(&NewAvd::for_profile(name, sysdir, &profile)).expect("create");
            env.load(name).expect("load")
        };
        let hw =
            |avd: &Avd| ["hw.mainKeys", "hw.dPad", "hw.keyboard", "hw.trackBall"].map(|k| avd.get(k).unwrap_or("-").to_string()).join(" ");
        let phone_mask = 0x7ff | (1 << 17);

        let phone = create("phone", FormFactor::Phone, &image("google_apis", 36));
        assert_eq!(hw(&phone), "no no yes no");
        assert_eq!((phone.main_keys(), phone.circular(), phone.ram_bytes()), (false, false, 2 << 30));
        assert_eq!(phone.qemu_props().last(), Some(&("qemu.hw.mainkeys".to_string(), "0".to_string())), "a navigation bar");
        assert!(!phone.dir.join("boot.prop").exists());
        assert_eq!(phone.sensor_mask(), phone_mask);

        // Wear: 512 MB (booted as 1 GB), hardware keys, round, heart rate and wrist tilt.
        let watch = create("watch", FormFactor::Watch, &image("android-wear", 34));
        assert_eq!(hw(&watch), "yes no yes no");
        assert_eq!((watch.get("hw.ramSize"), watch.ram_bytes()), (Some("512"), 1 << 30));
        assert_eq!(watch.boot_properties(), [("ro.emulator.circular".to_string(), "true".to_string())]);
        assert!(watch.main_keys() && watch.circular());
        let props: Vec<String> = watch.qemu_props().iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(props, ["ro.emulator.circular=true", "qemu.sf.lcd_density=320", "qemu.hw.mainkeys=1"]);
        assert_eq!(watch.sensor_mask(), phone_mask | (1 << 14) | (1 << 16));
        assert_eq!(watch.get("hw.device.name"), Some("wearos_large_round"));

        // TV (the SDK's tv_1080p): no touch screen, light and GPS only, mains power.
        let tv = create("tv", FormFactor::Tv, &image("google-tv", 34));
        assert_eq!(hw(&tv), "yes yes yes no");
        assert_eq!((tv.profile().kind.form_factor, tv.ram_bytes(), tv.circular()), (FormFactor::Tv, 2 << 30, false));
        assert_eq!([tv.get("hw.device.name"), tv.get("hw.screen"), tv.get("hw.battery")], [Some("tv_1080p"), Some("no-touch"), Some("no")]);
        let sensors = |avd: &Avd| {
            let mask = avd.sensor_mask();
            sim_core::goldfish::SENSORS.iter().enumerate().filter(|(h, _)| mask & (1 << h) != 0).map(|(_, n)| *n).collect::<Vec<_>>()
        };
        let calibrated = |avd: &Avd| sensors(avd).into_iter().filter(|n| !n.ends_with("-uncalibrated")).collect::<Vec<_>>();
        assert_eq!(calibrated(&tv), ["temperature", "light", "humidity"], "no motion sensors");

        // Automotive (automotive_1024p_landscape): its 3686 MB, light and GPS, on-screen navigation.
        let car = create("car", FormFactor::Auto, &image("android-automotive", 34));
        assert_eq!(hw(&car), "no no yes no");
        assert_eq!((car.profile().kind.form_factor, car.ram_bytes()), (FormFactor::Auto, 3686 << 20));
        assert_eq!(calibrated(&car), ["light"]);
        assert_eq!(car.get("hw.gps"), Some("yes"));

        // Each key once: the device's values replace the phone defaults.
        let config = std::fs::read_to_string(car.dir.join("config.ini")).expect("config");
        assert_eq!(config.lines().filter(|l| l.starts_with("hw.mainKeys=")).count(), 1, "{config}");

        let fold = create("fold", FormFactor::Foldable, &image("google_apis", 36));
        assert_eq!(fold.sensor_mask(), phone_mask | (1 << 11), "the hinge reaches the HAL");
        assert_eq!(hw(&fold), "no no yes no");

        // An AVD without boot.prop falls back to the image's; `hw.mainKeys` defaults to yes.
        let mut bare = phone.clone();
        Arc::make_mut(&mut bare.config).remove("hw.mainKeys");
        assert!(bare.main_keys(), "stock's default");
        std::fs::write(bare.sysdir.join("boot.prop"), "ro.emulator.circular=1\n").expect("write");
        assert!(bare.circular());
        std::fs::write(bare.dir.join("boot.prop"), "ro.emulator.circular=no\n").expect("write");
        assert!(!bare.circular(), "the AVD's boot.prop wins");
        Arc::make_mut(&mut bare.config).insert("hw.lcd.circular".into(), "yes".into());
        assert!(bare.circular());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
