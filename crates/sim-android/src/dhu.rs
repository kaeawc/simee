//! Android Auto projection (#437, docs/research/car-pairing.md): Google's Desktop Head Unit (DHU)
//! run against a phone, and driven through its text console.
//!
//! The phone runs the app and the DHU shows it. An emulated phone has no USB, so the DHU reaches
//! the head unit server in the phone's Android Auto app through an adb forward:
//! `adb -s emulator-<n> forward tcp:<free> tcp:5277`, then `desktop-head-unit --adb=<free>`. Each
//! phone gets its own free host port, so several phones can each have a head unit.
//!
//! The first half is pure: the typed console commands ([`Command`]), the `.ini` ([`Config`]), the
//! `vehicle` mapping ([`vehicle`]) and the readers of what the phone says
//! ([`parse_version_name`], [`server_listening`]). The second half supervises the process:
//!
//! - the DHU is started with its stdin on a FIFO (`<avd>/simee/dhu.in`) that the child itself holds
//!   open for writing, so it never sees end of input and a later call, from another process, can
//!   still write a command line to it; stdout and stderr go to `dhu.log` beside it;
//! - its PID and what it was started with are recorded in `<avd>/simee/dhu.state` (as `emu`'s
//!   PID is), so [`Device::car_stop`] from any process finds it; a PID is signalled only while its
//!   command line still names the DHU, never a reused one;
//! - `--headless` still renders for `screenshot`: unverified, a device check (#438).
//!
//! The DHU's files are never bundled or fetched here (`emu -provision-dhu` installs them into the
//! SDK, under Google's terms): this module only runs `<sdk>/extras/google/auto/desktop-head-unit`.

use std::collections::HashMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::apps::Adb;
use crate::{Android, Device};

/// The port of the head unit server in the phone's Android Auto app (the forward's device end).
pub const SERVER_PORT: u16 = 5277;
/// Android Auto's package on the phone.
pub const ANDROID_AUTO: &str = "com.google.android.projection.gearhead";
/// The oldest Android Auto this module drives (major, minor): the head unit server and developer
/// mode of the DHU 2.x era. Default, unverified on a device.
pub const MIN_ANDROID_AUTO: (u32, u32) = (6, 0);
/// The DHU's program in an SDK, under its root (`extras;google;auto`).
pub const EXECUTABLE: &str = "extras/google/auto/desktop-head-unit";
/// The default dpi of a projection.
pub const DEFAULT_DPI: u32 = 160;
/// What a `screenshot` waits for the DHU to write its PNG.
pub const SHOT_TIMEOUT: Duration = Duration::from_secs(10);

/// The resolutions the DHU takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Resolution {
    R800x480,
    #[default]
    R1280x720,
    R1920x1080,
}

impl Resolution {
    pub const NAMES: &'static str = "800x480, 1280x720 or 1920x1080";

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "800x480" => Ok(Self::R800x480),
            "1280x720" => Ok(Self::R1280x720),
            "1920x1080" => Ok(Self::R1920x1080),
            other => Err(format!("unknown resolution `{other}` ({})", Self::NAMES)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::R800x480 => "800x480",
            Self::R1280x720 => "1280x720",
            Self::R1920x1080 => "1920x1080",
        }
    }

    /// Width and height in pixels, the space `tap` and `swipe` take on the car display.
    pub fn size(self) -> (u32, u32) {
        match self {
            Self::R800x480 => (800, 480),
            Self::R1280x720 => (1280, 720),
            Self::R1920x1080 => (1920, 1080),
        }
    }
}

/// The head unit's input: a touch screen, a rotary knob, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Touch,
    Rotary,
    Hybrid,
}

impl InputMode {
    pub const NAMES: &'static str = "touch, rotary or hybrid";

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "touch" => Ok(Self::Touch),
            "rotary" => Ok(Self::Rotary),
            "hybrid" => Ok(Self::Hybrid),
            other => Err(format!("unknown input `{other}` ({})", Self::NAMES)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Touch => "touch",
            Self::Rotary => "rotary",
            Self::Hybrid => "hybrid",
        }
    }

    /// Whether the screen takes touches (`tap`, `swipe`).
    pub fn touches(self) -> bool {
        self != Self::Rotary
    }
}

/// A sensor the DHU's `[sensors]` section enables; its console commands need it on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensor {
    Location,
    Speed,
    Gear,
    ParkingBrake,
    Night,
}

impl Sensor {
    pub const ALL: [Sensor; 5] = [Self::Location, Self::Speed, Self::Gear, Self::ParkingBrake, Self::Night];

    /// The key in the `.ini` and the name in `dhu.state`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Location => "location",
            Self::Speed => "speed",
            Self::Gear => "gear",
            Self::ParkingBrake => "parking_brake",
            Self::Night => "night",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.key() == key)
    }
}

/// What a projection is started with: the `.ini` the DHU reads (`-c`) and its input mode (`-i`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub resolution: Resolution,
    pub dpi: u32,
    pub input: InputMode,
    /// The sensors on; every one a [`Command`] needs by default.
    pub sensors: Vec<Sensor>,
}

impl Default for Config {
    fn default() -> Self {
        Config { resolution: Resolution::default(), dpi: DEFAULT_DPI, input: InputMode::default(), sensors: Sensor::ALL.to_vec() }
    }
}

impl Config {
    /// The dpi range accepted.
    pub const DPI: std::ops::RangeInclusive<u32> = 80..=640;

    /// Checks the dpi.
    pub fn validate(&self) -> Result<(), String> {
        if Self::DPI.contains(&self.dpi) {
            Ok(())
        } else {
            Err(format!("dpi {} is out of range ({}-{})", self.dpi, Self::DPI.start(), Self::DPI.end()))
        }
    }

    /// The `.ini` file: `[general]` with the resolution and dpi, `[sensors]` with each one on.
    pub fn ini(&self) -> String {
        let mut out = format!("[general]\nresolution={}\ndpi={}\n\n[sensors]\n", self.resolution.as_str(), self.dpi);
        for s in &self.sensors {
            out.push_str(&format!("{}=true\n", s.key()));
        }
        out
    }

    /// Checks a command against this projection: points on the screen, a touch on a touch
    /// screen, a sensor that is on. The line it would send, when it passes.
    pub fn line(&self, command: &Command) -> Result<String, String> {
        let (w, h) = self.resolution.size();
        let on_screen = |(x, y): (u32, u32)| {
            if x < w && y < h { Ok(()) } else { Err(format!("({x}, {y}) is off the {w}x{h} car display")) }
        };
        match command {
            Command::Tap { x, y } => on_screen((*x, *y))?,
            Command::Swipe { from, to } => {
                on_screen(*from)?;
                on_screen(*to)?;
            }
            _ => {}
        }
        if matches!(command, Command::Tap { .. } | Command::Swipe { .. }) && !self.input.touches() {
            return Err(format!(
                "the head unit was started with input={}, which has no touch screen: start it with touch or hybrid",
                self.input.as_str()
            ));
        }
        if let Some(sensor) = command.sensor()
            && !self.sensors.contains(&sensor)
        {
            return Err(format!("the `{}` sensor is off in this head unit's config", sensor.key()));
        }
        command.line()
    }
}

/// A media or navigation key (`keycode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Home,
    Back,
    Call,
    Search,
    Navigation,
    Tel,
    MediaPlay,
    MediaPause,
    MediaNext,
    MediaPrevious,
}

impl Key {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "home" => Self::Home,
            "back" => Self::Back,
            "call" => Self::Call,
            "search" => Self::Search,
            "navigation" => Self::Navigation,
            "tel" => Self::Tel,
            "media_play" => Self::MediaPlay,
            "media_pause" => Self::MediaPause,
            "media_next" => Self::MediaNext,
            "media_previous" => Self::MediaPrevious,
            other => return Err(format!("unknown keycode `{other}`")),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Back => "back",
            Self::Call => "call",
            Self::Search => "search",
            Self::Navigation => "navigation",
            Self::Tel => "tel",
            Self::MediaPlay => "media_play",
            Self::MediaPause => "media_pause",
            Self::MediaNext => "media_next",
            Self::MediaPrevious => "media_previous",
        }
    }
}

/// A rotary controller direction (`dpad`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dpad {
    Up,
    Down,
    Left,
    Right,
    Select,
}

impl Dpad {
    fn name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Select => "select",
        }
    }
}

/// What `focus` gives or takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Video,
    Audio,
    Nav,
}

impl Focus {
    fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Nav => "nav",
        }
    }
}

/// A gear (`gear`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gear {
    Park,
    Reverse,
    Neutral,
    Drive,
    /// A numbered gear, 1 to 9.
    Number(u8),
}

impl Gear {
    pub const NAMES: &'static str = "park, reverse, neutral, drive or 1-9";

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "park" | "p" => Ok(Self::Park),
            "reverse" | "r" => Ok(Self::Reverse),
            "neutral" | "n" => Ok(Self::Neutral),
            "drive" | "d" => Ok(Self::Drive),
            n => n
                .parse::<u8>()
                .ok()
                .filter(|n| (1..=9).contains(n))
                .map(Self::Number)
                .ok_or_else(|| format!("unknown gear `{s}` ({})", Self::NAMES)),
        }
    }

    fn word(self) -> String {
        match self {
            Self::Park => "park".into(),
            Self::Reverse => "reverse".into(),
            Self::Neutral => "neutral".into(),
            Self::Drive => "drive".into(),
            Self::Number(n) => n.to_string(),
        }
    }
}

/// One line of the DHU's stdin console, typed.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `tap x y`, in car display pixels.
    Tap {
        x: u32,
        y: u32,
    },
    /// `swipe x1 y1 x2 y2`. Not in Google's documented list: unverified on a DHU (a device check).
    Swipe {
        from: (u32, u32),
        to: (u32, u32),
    },
    Keycode(Key),
    Dpad(Dpad),
    /// `screenshot <png>`: the DHU writes the file itself.
    Screenshot(PathBuf),
    Day,
    Night,
    /// `daynight`: the DHU's own day-night cycle.
    DayNight,
    Focus {
        target: Focus,
        on: bool,
    },
    /// `restrict all` or `restrict none`: the driving restrictions.
    Restrict(bool),
    /// `speed <m/s>`.
    Speed(f64),
    Gear(Gear),
    ParkingBrake(bool),
    Location {
        lat: f64,
        lon: f64,
    },
    /// `mic play <wav>`.
    MicPlay(PathBuf),
    /// `sleep <seconds>`: for scripts.
    Sleep(f64),
}

impl Command {
    /// The sensor that must be on for the DHU to take the command.
    pub fn sensor(&self) -> Option<Sensor> {
        match self {
            Self::Speed(_) => Some(Sensor::Speed),
            Self::Gear(_) => Some(Sensor::Gear),
            Self::ParkingBrake(_) => Some(Sensor::ParkingBrake),
            Self::Location { .. } => Some(Sensor::Location),
            Self::Day | Self::Night | Self::DayNight => Some(Sensor::Night),
            _ => None,
        }
    }

    /// The console line, without its newline, after checking its arguments: numbers are finite and
    /// in range, a path is one word with no control characters (a newline would be a second
    /// command).
    pub fn line(&self) -> Result<String, String> {
        Ok(match self {
            Self::Tap { x, y } => format!("tap {x} {y}"),
            Self::Swipe { from, to } => format!("swipe {} {} {} {}", from.0, from.1, to.0, to.1),
            Self::Keycode(k) => format!("keycode {}", k.name()),
            Self::Dpad(d) => format!("dpad {}", d.name()),
            Self::Screenshot(p) => format!("screenshot {}", word(p)?),
            Self::Day => "day".into(),
            Self::Night => "night".into(),
            Self::DayNight => "daynight".into(),
            Self::Focus { target, on } => format!("focus {} {}", target.name(), if *on { "on" } else { "off" }),
            Self::Restrict(all) => format!("restrict {}", if *all { "all" } else { "none" }),
            Self::Speed(v) => {
                if !v.is_finite() || !(0.0..=200.0).contains(v) {
                    return Err(format!("speed {v} m/s is out of range (0-200)"));
                }
                format!("speed {v}")
            }
            Self::Gear(g) => format!("gear {}", g.word()),
            Self::ParkingBrake(on) => format!("parking_brake {}", if *on { "on" } else { "off" }),
            Self::Location { lat, lon } => {
                if !lat.is_finite() || !(-90.0..=90.0).contains(lat) || !lon.is_finite() || !(-180.0..=180.0).contains(lon) {
                    return Err(format!("location {lat},{lon} is out of range (latitude -90..90, longitude -180..180)"));
                }
                format!("location {lat} {lon}")
            }
            Self::MicPlay(p) => format!("mic play {}", word(p)?),
            Self::Sleep(s) => {
                if !s.is_finite() || !(0.0..=60.0).contains(s) {
                    return Err(format!("sleep {s} s is out of range (0-60)"));
                }
                format!("sleep {s}")
            }
        })
    }
}

/// A path as one console word.
fn word(p: &Path) -> Result<String, String> {
    let s = p.to_str().ok_or("the path isn't UTF-8")?;
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("`{s}` can't be a console argument: the DHU's console splits words at whitespace"));
    }
    Ok(s.to_owned())
}

/// The `vehicle` properties a head unit maps.
pub const VEHICLE_PROPERTIES: &[&str] = &["speed", "gear", "night", "parking-brake"];

/// `vehicle`'s property and value as a DHU command: `speed` (m/s, as the vehicle HAL's), `gear`
/// (park, reverse, neutral, drive, 1-9), `night` (true or false, or `day`/`night`) and
/// `parking-brake` (true or false). The name is read case-insensitively, `_` or `-`.
pub fn vehicle(property: &str, value: &str) -> Result<Command, String> {
    let name = property.to_ascii_lowercase().replace('_', "-");
    let flag = |v: &str| match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        other => Err(format!("`{other}` isn't true or false")),
    };
    match name.as_str() {
        "speed" => {
            value.trim().parse::<f64>().map(Command::Speed).map_err(|_| format!("speed `{value}` isn't a number (metres per second)"))
        }
        "gear" => Gear::parse(value.trim()).map(Command::Gear),
        "night" => match value.trim().to_ascii_lowercase().as_str() {
            "day" => Ok(Command::Day),
            "night" => Ok(Command::Night),
            v => flag(v).map(|n| if n { Command::Night } else { Command::Day }),
        },
        "parking-brake" => flag(value.trim()).map(Command::ParkingBrake),
        other => Err(format!(
            "a head unit takes {} (the DHU's console has no `{other}`); a device with Automotive's vehicle HAL takes any property",
            VEHICLE_PROPERTIES.join(", ")
        )),
    }
}

/// Android Auto's `versionName` in `dumpsys package` output.
pub fn parse_version_name(dumpsys: &str) -> Option<String> {
    dumpsys.lines().find_map(|l| l.trim().strip_prefix("versionName=")).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Whether version text like `14.2.640004-rc` is at least `min` (major, minor): its first two
/// numeric parts; a version it can't read is not.
pub fn version_at_least(version: &str, min: (u32, u32)) -> bool {
    let mut parts = version.split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten().or(Some(0))) {
        (Some(major), Some(minor)) => (major, minor) >= min,
        _ => false,
    }
}

/// Whether `/proc/net/tcp` (and `tcp6`) text has a socket listening on the head unit server's port.
pub fn server_listening(proc_net_tcp: &str) -> bool {
    let port = format!(":{:04X}", SERVER_PORT);
    proc_net_tcp.lines().any(|l| {
        let mut f = l.split_whitespace();
        let (local, state) = (f.nth(1), f.nth(1));
        local.is_some_and(|a| a.to_ascii_uppercase().ends_with(&port)) && state == Some("0A")
    })
}

/// `bytes`' PNG size, from its header.
pub fn png_size(bytes: &[u8]) -> Result<(u32, u32), String> {
    if bytes.len() < 24 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return Err("the DHU's screenshot isn't a PNG".into());
    }
    let be = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Ok((be(16), be(20)))
}

// ---- the supervisor ----

/// What `car_start` asks for.
#[derive(Clone, Debug)]
pub struct Options {
    pub config: Config,
    /// Run without the DHU's window (`--headless`).
    pub headless: bool,
    /// How long the DHU must stay up for the start to count: one that dials a phone with no head
    /// unit server exits within this.
    pub startup: Duration,
    /// How long a stop waits after SIGTERM before SIGKILL.
    pub grace: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options { config: Config::default(), headless: false, startup: Duration::from_millis(1000), grace: Duration::from_secs(3) }
    }
}

/// A running projection, as `dhu.state` records it.
#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    pub pid: u32,
    /// The phone's adb serial the forward is on.
    pub serial: String,
    /// The host port forwarded to the phone's [`SERVER_PORT`].
    pub forward_port: u16,
    pub config: Config,
    pub headless: bool,
}

impl Projection {
    /// `dhu.state`'s text: one `key=value` per line.
    pub fn render(&self) -> String {
        let sensors: Vec<&str> = self.config.sensors.iter().map(|s| s.key()).collect();
        format!(
            "pid={}\nserial={}\nforward_port={}\nresolution={}\ndpi={}\ninput={}\nheadless={}\nsensors={}\n",
            self.pid,
            self.serial,
            self.forward_port,
            self.config.resolution.as_str(),
            self.config.dpi,
            self.config.input.as_str(),
            self.headless,
            sensors.join(",")
        )
    }

    pub fn parse(text: &str) -> Option<Self> {
        let kv: HashMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
        Some(Projection {
            pid: kv.get("pid")?.parse().ok()?,
            serial: (*kv.get("serial")?).to_owned(),
            forward_port: kv.get("forward_port")?.parse().ok()?,
            config: Config {
                resolution: Resolution::parse(kv.get("resolution")?).ok()?,
                dpi: kv.get("dpi")?.parse().ok()?,
                input: InputMode::parse(kv.get("input")?).ok()?,
                sensors: kv.get("sensors")?.split(',').filter_map(Sensor::from_key).collect(),
            },
            headless: *kv.get("headless")? == "true",
        })
    }
}

/// What a stop did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stopped {
    pub pid: u32,
    /// Whether the DHU was still running (else its record was stale).
    pub was_running: bool,
    /// Whether the adb forward was removed (it can't be once the phone is gone).
    pub forward_removed: bool,
}

/// Children this process started, kept so a stopped one is reaped (a zombie still answers
/// signal 0).
static CHILDREN: Mutex<Option<HashMap<u32, Child>>> = Mutex::new(None);

fn children<R>(f: impl FnOnce(&mut HashMap<u32, Child>) -> R) -> R {
    let mut guard = CHILDREN.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Whether process `pid` is still running.
fn pid_alive(pid: u32) -> bool {
    let ours = children(|c| c.get_mut(&pid).map(|child| child.try_wait().map(|s| s.is_none()).unwrap_or(false)));
    if let Some(alive) = ours {
        if !alive {
            children(|c| c.remove(&pid));
        }
        return alive;
    }
    Process::new("kill").args(["-0", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Whether `pid`'s command line names the DHU: the check before signalling a recorded PID.
fn is_head_unit(pid: u32) -> bool {
    Process::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("desktop-head-unit"))
}

fn signal(name: &str, pid: u32) {
    let _ = Process::new("kill").args([name, &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("dhu.state")
}
fn fifo_path(dir: &Path) -> PathBuf {
    dir.join("dhu.in")
}
fn log_path(dir: &Path) -> PathBuf {
    dir.join("dhu.log")
}

/// A free TCP port on the loopback (bound and released: another process could take it before
/// the forward does, which the forward's failure reports).
fn free_port() -> Result<u16, String> {
    let l =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("no free port for the adb forward: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

/// The last lines of the DHU's log, for an error.
fn log_tail(dir: &Path) -> String {
    let text = std::fs::read_to_string(log_path(dir)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(5);
    if lines.is_empty() { "it printed nothing".to_owned() } else { lines[from..].join(" | ") }
}

/// The recorded projection in `dir` (`<avd>/simee`) when its DHU still runs; a stale record is
/// `None` (and is left for [`stop`] to clear).
pub fn running(dir: &Path) -> Option<Projection> {
    let p = Projection::parse(&std::fs::read_to_string(state_path(dir)).ok()?)?;
    pid_alive(p.pid).then_some(p)
}

fn forward_remove(adb: &Adb, port: u16) -> bool {
    adb.run(&["forward", "--remove", &format!("tcp:{port}")], adb.deadlines.query).is_ok_and(|(ok, _)| ok)
}

/// Starts the DHU `program` against the phone `adb` is bound to, with state in `dir`.
pub fn start(program: &Path, adb: &Adb, dir: &Path, options: &Options) -> Result<Projection, String> {
    if cfg!(windows) {
        return Err("Android Auto projection isn't supported on Windows yet: the DHU console needs a FIFO (unix)".into());
    }
    options.config.validate()?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if let Some(p) = running(dir) {
        return Err(format!(
            "a head unit is already running for this device (PID {}, port {}): `car_stop` it first",
            p.pid, p.forward_port
        ));
    }
    let port = free_port()?;
    let (ok, text) = adb.run(&["forward", &format!("tcp:{port}"), &format!("tcp:{SERVER_PORT}")], adb.deadlines.query)?;
    if !ok {
        return Err(crate::apps::failure("adb forward", &text));
    }
    let started = spawn(program, dir, port, options).and_then(|child| {
        let pid = child.id();
        children(|c| c.insert(pid, child));
        let deadline = Instant::now() + options.startup;
        while Instant::now() < deadline {
            let exited = children(|c| c.get_mut(&pid).and_then(|ch| ch.try_wait().ok().flatten()));
            if let Some(status) = exited {
                children(|c| c.remove(&pid));
                return Err(format!(
                    "the Desktop Head Unit exited ({status}) right after starting: {}. It needs the head unit server running on the phone \
                     (Android Auto's developer menu, \"Start head unit server\")",
                    log_tail(dir)
                ));
            }
            sleep(Duration::from_millis(25));
        }
        Ok(Projection { pid, serial: adb.serial.clone(), forward_port: port, config: options.config.clone(), headless: options.headless })
    });
    match started {
        Ok(p) => {
            std::fs::write(state_path(dir), p.render()).map_err(|e| format!("{}: {e}", state_path(dir).display()))?;
            Ok(p)
        }
        Err(e) => {
            forward_remove(adb, port);
            let _ = std::fs::remove_file(fifo_path(dir));
            Err(e)
        }
    }
}

fn spawn(program: &Path, dir: &Path, port: u16, options: &Options) -> Result<Child, String> {
    let ini = dir.join("dhu.ini");
    std::fs::write(&ini, options.config.ini()).map_err(|e| format!("{}: {e}", ini.display()))?;
    let fifo = fifo_path(dir);
    let _ = std::fs::remove_file(&fifo);
    let made = Process::new("mkfifo").arg(&fifo).status().map_err(|e| format!("mkfifo: {e}"))?;
    if !made.success() {
        return Err(format!("couldn't make {}", fifo.display()));
    }
    // Read and write, so opening never waits for the other end and the child, which inherits this
    // handle as its stdin, is its own writer: it never reads end of input.
    let stdin = OpenOptions::new().read(true).write(true).open(&fifo).map_err(|e| format!("{}: {e}", fifo.display()))?;
    let log = OpenOptions::new().create(true).append(true).open(log_path(dir)).map_err(|e| format!("{}: {e}", log_path(dir).display()))?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = Process::new(program);
    command.arg(format!("--adb={port}")).arg("-i").arg(options.config.input.as_str()).arg("-c").arg(&ini);
    if options.headless {
        command.arg("--headless");
    }
    if let Some(parent) = program.parent().filter(|p| !p.as_os_str().is_empty()) {
        command.current_dir(parent);
    }
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| format!("couldn't run {}: {e}", program.display()))
}

/// Stops the recorded DHU in `dir` and removes its forward and its files. A DHU already gone
/// still clears its record; a PID that no longer names the DHU is never signalled.
pub fn stop(dir: &Path, adb: Option<&Adb>, grace: Duration) -> Result<Stopped, String> {
    let text =
        std::fs::read_to_string(state_path(dir)).map_err(|_| "no head unit is running for this device (nothing to stop)".to_owned())?;
    let p = Projection::parse(&text).ok_or_else(|| format!("{} is unreadable; delete it", state_path(dir).display()))?;
    let was_running = pid_alive(p.pid);
    let ours = children(|c| c.contains_key(&p.pid));
    if was_running && !ours && !is_head_unit(p.pid) {
        return Err(format!(
            "PID {} is running but isn't a Desktop Head Unit any more (its command line changed); not signalling it. Remove {} to forget it",
            p.pid,
            state_path(dir).display()
        ));
    }
    if was_running {
        signal("-TERM", p.pid);
        let deadline = Instant::now() + grace;
        while pid_alive(p.pid) && Instant::now() < deadline {
            sleep(Duration::from_millis(25));
        }
        if pid_alive(p.pid) {
            signal("-KILL", p.pid);
            let deadline = Instant::now() + Duration::from_secs(2);
            while pid_alive(p.pid) && Instant::now() < deadline {
                sleep(Duration::from_millis(25));
            }
        }
    }
    children(|c| c.remove(&p.pid).map(|mut child| child.wait()));
    if pid_alive(p.pid) {
        return Err(format!("the Desktop Head Unit (PID {}) is still running after SIGKILL", p.pid));
    }
    let forward_removed = adb.is_some_and(|a| forward_remove(a, p.forward_port));
    for f in [state_path(dir), fifo_path(dir)] {
        let _ = std::fs::remove_file(f);
    }
    Ok(Stopped { pid: p.pid, was_running, forward_removed })
}

/// Writes `command` to the running DHU's console, after checking it against its config.
pub fn send(dir: &Path, command: &Command) -> Result<(), String> {
    let p = running(dir).ok_or_else(no_head_unit)?;
    let line = p.config.line(command)?;
    let mut fifo = OpenOptions::new().read(true).write(true).open(fifo_path(dir)).map_err(|e| format!("the DHU's console: {e}"))?;
    fifo.write_all(format!("{line}\n").as_bytes()).map_err(|e| format!("the DHU's console: {e}"))
}

/// The error for a call that needs a head unit when none runs.
pub fn no_head_unit() -> String {
    "no head unit is running for this device: start one with `car_start`".to_owned()
}

/// Has the DHU write a screenshot to `target` (absolute) and waits for the whole file.
pub fn screenshot_to(dir: &Path, target: &Path, timeout: Duration) -> Result<(u32, u32), String> {
    let _ = std::fs::remove_file(target);
    send(dir, &Command::Screenshot(target.to_path_buf()))?;
    let deadline = Instant::now() + timeout;
    let mut last = 0;
    while Instant::now() < deadline {
        if let Ok(meta) = std::fs::metadata(target)
            && meta.len() > 0
        {
            // Complete when it stops growing and reads as a PNG.
            if meta.len() == last && std::fs::read(target).ok().and_then(|b| png_size(&b).ok()).is_some() {
                return png_size(&std::fs::read(target).map_err(|e| e.to_string())?);
            }
            last = meta.len();
        }
        if running(dir).is_none() {
            return Err(format!("the Desktop Head Unit exited before it wrote the screenshot: {}", log_tail(dir)));
        }
        sleep(Duration::from_millis(40));
    }
    Err(format!(
        "the Desktop Head Unit wrote no screenshot in {timeout:?} (is it connected to the phone? `--headless` may not render: see {})",
        log_path(dir).display()
    ))
}

// ---- on a device ----

impl Adb {
    /// Android Auto's `versionName` on the phone, `None` when it isn't installed.
    pub fn android_auto_version(&self) -> Result<Option<String>, String> {
        let out = self.shell(&format!("dumpsys package {ANDROID_AUTO}"), self.deadlines.query)?;
        Ok(parse_version_name(&out))
    }

    /// Whether the head unit server listens on the phone.
    pub fn head_unit_server_listening(&self) -> Result<bool, String> {
        Ok(server_listening(&self.shell("cat /proc/net/tcp /proc/net/tcp6", self.deadlines.query)?))
    }
}

impl Device {
    fn simee_dir(&self) -> Result<PathBuf, String> {
        Ok(self.avd()?.dir.join("simee"))
    }

    /// The head unit running for this device, if any.
    pub fn head_unit(&self) -> Result<Option<Projection>, String> {
        Ok(running(&self.simee_dir()?))
    }

    /// Starts a head unit: the DHU is found in the SDK, Android Auto and its server are checked on
    /// the phone (each missing piece answers with what to do), a port is forwarded and the DHU
    /// spawned and supervised.
    pub fn car_start(&self, android: &Android, options: &Options) -> Result<Projection, String> {
        let sdk = android.env.sdk_root.as_ref().ok_or("no Android SDK found: the Desktop Head Unit lives in it")?;
        let program = sdk.join(EXECUTABLE);
        if !program.is_file() {
            return Err(format!(
                "the Desktop Head Unit isn't installed ({} is missing): `emu -provision-dhu -accept-licenses` installs it",
                program.display()
            ));
        }
        let adb = self.adb(android)?;
        let name = &self.info.name;
        match adb.android_auto_version()? {
            None => {
                return Err(format!(
                    "Android Auto isn't installed on {name} ({ANDROID_AUTO}): the DHU needs it. Install it from the Play Store (an image with \
                     Play) or with `install`"
                ));
            }
            Some(v) if !version_at_least(&v, MIN_ANDROID_AUTO) => {
                return Err(format!(
                    "Android Auto {v} on {name} is too old for the Desktop Head Unit (it needs {}.{} or newer): update it in the Play Store",
                    MIN_ANDROID_AUTO.0, MIN_ANDROID_AUTO.1
                ));
            }
            Some(_) => {}
        }
        if !adb.head_unit_server_listening()? {
            return Err(format!(
                "no head unit server is running on {name} (nothing listens on port {SERVER_PORT}): in Android Auto's settings tap Version ten \
                 times for developer mode, then the menu's \"Start head unit server\", with the screen unlocked"
            ));
        }
        start(&program, &adb, &self.simee_dir()?, options)
    }

    /// Stops this device's head unit and removes its forward.
    pub fn car_stop(&self, android: &Android) -> Result<Stopped, String> {
        let adb = self.adb(android).ok();
        stop(&self.simee_dir()?, adb.as_ref(), Options::default().grace)
    }

    /// Sends a console command to this device's head unit.
    pub fn car_command(&self, command: &Command) -> Result<(), String> {
        send(&self.simee_dir()?, command)
    }

    /// The car display as the DHU's PNG: its size and bytes.
    pub fn car_screenshot(&self) -> Result<(u32, u32, Vec<u8>), String> {
        let dir = self.simee_dir()?;
        let tmp = dir.join(format!("car-shot-{}.png", std::process::id()));
        let result =
            screenshot_to(&dir, &tmp, SHOT_TIMEOUT).and_then(|(w, h)| std::fs::read(&tmp).map(|b| (w, h, b)).map_err(|e| e.to_string()));
        let _ = std::fs::remove_file(&tmp);
        result
    }

    /// The car display written to `path` by the DHU itself.
    pub fn car_screenshot_to(&self, path: &Path) -> Result<(u32, u32), String> {
        let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
        screenshot_to(&self.simee_dir()?, &absolute, SHOT_TIMEOUT)
    }
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DHU PID {} on {} (forward tcp:{} -> tcp:{SERVER_PORT})", self.pid, self.serial, self.forward_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_formatted_as_the_console_reads_them() {
        let line = |c: Command| c.line().expect("line");
        assert_eq!(line(Command::Tap { x: 10, y: 20 }), "tap 10 20");
        assert_eq!(line(Command::Swipe { from: (1, 2), to: (3, 4) }), "swipe 1 2 3 4");
        assert_eq!(line(Command::Keycode(Key::MediaNext)), "keycode media_next");
        assert_eq!(line(Command::Dpad(Dpad::Select)), "dpad select");
        assert_eq!(line(Command::Screenshot("/tmp/a.png".into())), "screenshot /tmp/a.png");
        assert_eq!(line(Command::Night), "night");
        assert_eq!(line(Command::DayNight), "daynight");
        assert_eq!(line(Command::Focus { target: Focus::Nav, on: false }), "focus nav off");
        assert_eq!(line(Command::Restrict(true)), "restrict all");
        assert_eq!(line(Command::Restrict(false)), "restrict none");
        assert_eq!(line(Command::Speed(12.5)), "speed 12.5");
        assert_eq!(line(Command::Gear(Gear::Number(3))), "gear 3");
        assert_eq!(line(Command::ParkingBrake(true)), "parking_brake on");
        assert_eq!(line(Command::Location { lat: 37.5, lon: -122.25 }), "location 37.5 -122.25");
        assert_eq!(line(Command::MicPlay("/tmp/q.wav".into())), "mic play /tmp/q.wav");
        assert_eq!(line(Command::Sleep(0.5)), "sleep 0.5");
    }

    #[test]
    fn arguments_are_checked() {
        let err = |c: Command| c.line().expect_err("refused");
        assert!(err(Command::Speed(-1.0)).contains("out of range"));
        assert!(err(Command::Speed(f64::NAN)).contains("out of range"));
        assert!(err(Command::Location { lat: 91.0, lon: 0.0 }).contains("out of range"));
        assert!(err(Command::Location { lat: 0.0, lon: f64::INFINITY }).contains("out of range"));
        assert!(err(Command::Sleep(61.0)).contains("out of range"));
        // A newline would be a second command.
        assert!(err(Command::Screenshot("/tmp/a.png\nquit".into())).contains("can't be a console argument"));
        assert!(err(Command::MicPlay("/tmp/my file.wav".into())).contains("whitespace"));
        assert!(err(Command::Screenshot(PathBuf::new())).contains("can't be a console argument"));
    }

    #[test]
    fn names_parse_and_unknown_ones_list_the_choices() {
        assert_eq!(Resolution::parse("1920x1080"), Ok(Resolution::R1920x1080));
        assert!(Resolution::parse("640x480").expect_err("unknown").contains("800x480, 1280x720 or 1920x1080"));
        assert_eq!(InputMode::parse("hybrid"), Ok(InputMode::Hybrid));
        assert!(InputMode::parse("voice").expect_err("unknown").contains("touch, rotary or hybrid"));
        assert_eq!(Gear::parse("D"), Ok(Gear::Drive));
        assert_eq!(Gear::parse("7"), Ok(Gear::Number(7)));
        assert!(Gear::parse("10").is_err() && Gear::parse("0").is_err() && Gear::parse("sport").is_err());
        assert_eq!(Key::parse("home"), Ok(Key::Home));
        assert!(Key::parse("volume_up").is_err());
    }

    #[test]
    fn the_ini_has_the_resolution_dpi_and_sensors() {
        let config = Config {
            resolution: Resolution::R800x480,
            dpi: 240,
            input: InputMode::Rotary,
            sensors: vec![Sensor::Speed, Sensor::ParkingBrake],
        };
        assert_eq!(config.ini(), "[general]\nresolution=800x480\ndpi=240\n\n[sensors]\nspeed=true\nparking_brake=true\n");
        let all = Config::default().ini();
        assert!(all.starts_with("[general]\nresolution=1280x720\ndpi=160\n"), "{all}");
        for s in Sensor::ALL {
            assert!(all.contains(&format!("{}=true", s.key())), "{all}");
        }
        assert!(Config { dpi: 20, ..Config::default() }.validate().expect_err("low").contains("out of range"));
        assert!(Config { dpi: 700, ..Config::default() }.validate().is_err());
        assert_eq!(Config::default().validate(), Ok(()));
    }

    #[test]
    fn a_config_checks_points_input_and_sensors() {
        let config = Config::default();
        assert_eq!(config.line(&Command::Tap { x: 1279, y: 719 }), Ok("tap 1279 719".into()));
        assert!(config.line(&Command::Tap { x: 1280, y: 0 }).expect_err("off").contains("off the 1280x720 car display"));
        assert!(config.line(&Command::Swipe { from: (0, 0), to: (0, 720) }).is_err());
        let rotary = Config { input: InputMode::Rotary, ..Config::default() };
        assert!(rotary.line(&Command::Tap { x: 1, y: 1 }).expect_err("no touch").contains("no touch screen"));
        assert_eq!(rotary.line(&Command::Dpad(Dpad::Up)), Ok("dpad up".into()));
        let no_speed = Config { sensors: vec![Sensor::Gear], ..Config::default() };
        assert!(no_speed.line(&Command::Speed(3.0)).expect_err("off").contains("`speed` sensor is off"));
        assert_eq!(no_speed.line(&Command::Gear(Gear::Drive)), Ok("gear drive".into()));
        assert!(no_speed.line(&Command::Day).is_err(), "day and night need the night sensor");
    }

    #[test]
    fn vehicle_maps_the_four_properties() {
        assert_eq!(vehicle("speed", "12.5"), Ok(Command::Speed(12.5)));
        assert_eq!(vehicle("GEAR", "drive"), Ok(Command::Gear(Gear::Drive)));
        assert_eq!(vehicle("night", "true"), Ok(Command::Night));
        assert_eq!(vehicle("night", "false"), Ok(Command::Day));
        assert_eq!(vehicle("night", "day"), Ok(Command::Day));
        assert_eq!(vehicle("parking-brake", "on"), Ok(Command::ParkingBrake(true)));
        assert_eq!(vehicle("PARKING_BRAKE", "0"), Ok(Command::ParkingBrake(false)));
        assert!(vehicle("speed", "fast").expect_err("nan").contains("isn't a number"));
        assert!(vehicle("night", "maybe").is_err());
        let err = vehicle("fuel", "1").expect_err("unmapped");
        assert!(err.contains("speed, gear, night, parking-brake"), "{err}");
    }

    #[test]
    fn android_auto_is_read_from_dumpsys() {
        let dumpsys = "Packages:\n  Package [com.google.android.projection.gearhead] (1a2b):\n    versionCode=1 minSdk=26\n    versionName=14.2.640004-rc\n";
        assert_eq!(parse_version_name(dumpsys), Some("14.2.640004-rc".into()));
        assert_eq!(parse_version_name("Unable to find package: com.google.android.projection.gearhead"), None);
        assert!(version_at_least("14.2.640004-rc", MIN_ANDROID_AUTO));
        assert!(version_at_least("6.0", (6, 0)) && version_at_least("6", (6, 0)));
        assert!(!version_at_least("5.9.1", (6, 0)));
        assert!(!version_at_least("beta", (6, 0)));
    }

    #[test]
    fn the_server_is_a_listener_on_5277() {
        let head = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let listening = format!("{head}   0: 0100007F:149D 00000000:0000 0A 00000000:00000000 00:00000000 00000000 10170 0 1 1 0\n");
        assert!(server_listening(&listening));
        let connected = format!("{head}   0: 0100007F:149D 0100007F:C350 01 00000000:00000000 00:00000000 00000000 10170 0 1 1 0\n");
        assert!(!server_listening(&connected), "a connection isn't a listener");
        let other = format!("{head}   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 1 1 0\n");
        assert!(!server_listening(&other));
        assert!(!server_listening(""));
    }

    #[test]
    fn a_state_file_round_trips() {
        let p = Projection {
            pid: 4242,
            serial: "emulator-5556".into(),
            forward_port: 41000,
            config: Config {
                resolution: Resolution::R1920x1080,
                dpi: 200,
                input: InputMode::Hybrid,
                sensors: vec![Sensor::Speed, Sensor::Night],
            },
            headless: true,
        };
        assert_eq!(Projection::parse(&p.render()), Some(p));
        assert_eq!(Projection::parse("pid=1\n"), None);
    }

    #[test]
    fn png_headers_give_the_size() {
        let png = sim_core::frame::encode_png(&sim_core::frame::Frame { width: 3, height: 2, rgba: vec![0; 24] }).expect("png");
        assert_eq!(png_size(&png), Ok((3, 2)));
        assert!(png_size(b"not a png at all, no").is_err());
    }

    /// The supervisor against a fake DHU (a shell script) and the fake adb.
    #[cfg(unix)]
    mod supervised {
        use super::*;
        use crate::testing::{fake_adb, write_script};

        /// An SDK with a fake adb and a fake DHU; the DHU records its arguments (`dhu.argv`) and
        /// each console line (`dhu.lines`), and writes `shot.png` for `screenshot <path>`.
        fn fixture(tag: &str, dhu_body: &str) -> (PathBuf, Adb, PathBuf, PathBuf) {
            let root = PathBuf::from("/tmp").join(format!("sa-dhu-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let android = Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
            let adb_log = fake_adb(&android);
            let _ = adb_log;
            let auto = root.join("sdk/extras/google/auto");
            std::fs::create_dir_all(&auto).expect("auto dir");
            let png = sim_core::frame::encode_png(&sim_core::frame::Frame { width: 1280, height: 720, rgba: vec![7; 1280 * 720 * 4] })
                .expect("png");
            std::fs::write(auto.join("shot.png"), png).expect("png");
            write_script(&auto.join("desktop-head-unit"), dhu_body);
            let adb = Adb {
                adb: root.join("sdk/platform-tools/adb"),
                serial: "emulator-5556".into(),
                bundletool: None,
                pid_wait: Duration::from_millis(100),
                deadlines: crate::apps::Deadlines::default(),
            };
            let dir = root.join("avd/phone.avd/simee");
            (root, adb, auto.join("desktop-head-unit"), dir)
        }

        pub(super) const FAKE_DHU: &str = r#"d="$(dirname "$0")"
echo "$*" > "$d/dhu.argv"
while read -r line; do
  echo "$line" >> "$d/dhu.lines"
  case "$line" in
    "screenshot "*) cp "$d/shot.png" "${line#screenshot }";;
  esac
done
"#;

        fn quick() -> Options {
            Options { startup: Duration::from_millis(300), grace: Duration::from_secs(2), ..Options::default() }
        }

        fn wait_lines(path: &Path, n: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let text = std::fs::read_to_string(path).unwrap_or_default();
                let lines: Vec<String> = text.lines().map(str::to_owned).collect();
                if lines.len() >= n || Instant::now() > deadline {
                    return lines;
                }
                sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn start_forwards_a_free_port_and_the_dhu_dials_it() {
            let (root, adb, program, dir) = fixture("start", FAKE_DHU);
            let options = Options { headless: true, config: Config { input: InputMode::Hybrid, dpi: 213, ..Config::default() }, ..quick() };
            let p = start(&program, &adb, &dir, &options).expect("start");
            assert!(p.forward_port > 0 && pid_alive(p.pid));
            assert_eq!(running(&dir), Some(p.clone()), "the record is on disk and its PID alive");
            let argv = wait_lines(&program.with_file_name("dhu.argv"), 1);
            assert_eq!(argv, vec![format!("--adb={} -i hybrid -c {} --headless", p.forward_port, dir.join("dhu.ini").display())]);
            let ini = std::fs::read_to_string(dir.join("dhu.ini")).expect("ini");
            assert!(ini.contains("resolution=1280x720") && ini.contains("dpi=213"), "{ini}");
            let calls = std::fs::read_to_string(root.join("sdk/platform-tools/adb.log")).expect("adb log");
            assert!(calls.contains(&format!("forward tcp:{} tcp:5277", p.forward_port)), "{calls}");
            // A second start is refused while the first runs.
            assert!(start(&program, &adb, &dir, &quick()).expect_err("running").contains("already running"));
            let stopped = stop(&dir, Some(&adb), Duration::from_secs(2)).expect("stop");
            assert_eq!((stopped.pid, stopped.was_running, stopped.forward_removed), (p.pid, true, true));
            assert!(!pid_alive(p.pid), "the process is gone and reaped");
            assert_eq!(running(&dir), None);
            assert!(!dir.join("dhu.state").exists() && !dir.join("dhu.in").exists());
            let calls = std::fs::read_to_string(root.join("sdk/platform-tools/adb.log")).expect("adb log");
            assert!(calls.contains(&format!("forward --remove tcp:{}", p.forward_port)), "{calls}");
            assert!(stop(&dir, Some(&adb), Duration::from_secs(1)).expect_err("nothing").contains("nothing to stop"));
            std::fs::remove_dir_all(root).ok();
        }

        #[test]
        fn two_phones_get_two_ports() {
            let (root, adb, program, dir) = fixture("two", FAKE_DHU);
            let other = root.join("avd/other.avd/simee");
            let a = start(&program, &adb, &dir, &quick()).expect("first");
            let b = start(&program, &Adb { serial: "emulator-5558".into(), ..adb.clone() }, &other, &quick()).expect("second");
            assert_ne!(a.forward_port, b.forward_port);
            assert_eq!(b.serial, "emulator-5558");
            stop(&dir, Some(&adb), Duration::from_secs(2)).expect("stop a");
            assert!(running(&other).is_some(), "stopping one leaves the other");
            stop(&other, Some(&adb), Duration::from_secs(2)).expect("stop b");
            std::fs::remove_dir_all(root).ok();
        }

        #[test]
        fn commands_reach_the_console_across_calls() {
            let (root, adb, program, dir) = fixture("send", FAKE_DHU);
            start(&program, &adb, &dir, &quick()).expect("start");
            send(&dir, &Command::Tap { x: 100, y: 200 }).expect("tap");
            send(&dir, &Command::Speed(9.5)).expect("speed");
            assert!(send(&dir, &Command::Tap { x: 5000, y: 0 }).is_err(), "refused before it is written");
            let lines = wait_lines(&program.with_file_name("dhu.lines"), 2);
            assert_eq!(lines, vec!["tap 100 200", "speed 9.5"]);
            stop(&dir, Some(&adb), Duration::from_secs(2)).expect("stop");
            assert!(send(&dir, &Command::Day).expect_err("stopped").contains("no head unit is running"));
            std::fs::remove_dir_all(root).ok();
        }

        #[test]
        fn a_screenshot_is_the_pngs_the_dhu_writes() {
            let (root, adb, program, dir) = fixture("shot", FAKE_DHU);
            start(&program, &adb, &dir, &quick()).expect("start");
            let target = root.join("car.png");
            assert_eq!(screenshot_to(&dir, &target, Duration::from_secs(5)), Ok((1280, 720)));
            assert!(png_size(&std::fs::read(&target).expect("file")).is_ok());
            stop(&dir, Some(&adb), Duration::from_secs(2)).expect("stop");
            std::fs::remove_dir_all(root).ok();
        }

        #[test]
        fn a_screenshot_that_never_comes_times_out_with_the_log() {
            let (root, adb, program, dir) = fixture("noshot", "echo 'rendering disabled' >&2\ncat > /dev/null\n");
            start(&program, &adb, &dir, &quick()).expect("start");
            let err = screenshot_to(&dir, &root.join("x.png"), Duration::from_millis(200)).expect_err("none");
            assert!(err.contains("wrote no screenshot") && err.contains("dhu.log"), "{err}");
            stop(&dir, Some(&adb), Duration::from_secs(2)).expect("stop");
            std::fs::remove_dir_all(root).ok();
        }

        #[test]
        fn a_dhu_that_exits_right_away_is_reported_with_its_output() {
            let (root, adb, program, dir) = fixture("exit", "echo 'Connection refused: no head unit server' >&2\nexit 3\n");
            let err = start(&program, &adb, &dir, &quick()).expect_err("exits");
            assert!(err.contains("exited") && err.contains("Connection refused") && err.contains("head unit server"), "{err}");
            assert_eq!(running(&dir), None);
            assert!(!dir.join("dhu.state").exists() && !dir.join("dhu.in").exists(), "nothing left behind");
            let calls = std::fs::read_to_string(root.join("sdk/platform-tools/adb.log")).expect("adb log");
            assert!(calls.contains("forward --remove"), "the forward is taken back: {calls}");
            std::fs::remove_dir_all(root).ok();
        }

        #[test]
        fn a_stale_record_is_cleared_and_a_reused_pid_is_never_signalled() {
            let (root, adb, _program, dir) = fixture("stale", FAKE_DHU);
            std::fs::create_dir_all(&dir).expect("dir");
            let mut p = Projection {
                pid: 999_999,
                serial: "emulator-5556".into(),
                forward_port: 40001,
                config: Config::default(),
                headless: false,
            };
            std::fs::write(dir.join("dhu.state"), p.render()).expect("state");
            assert_eq!(running(&dir), None, "a dead PID isn't a running head unit");
            let stopped = stop(&dir, Some(&adb), Duration::from_secs(1)).expect("clears");
            assert!(!stopped.was_running && !dir.join("dhu.state").exists());
            // A live process that isn't the DHU (a sleep this test started) is left alone.
            let mut other = Process::new("sleep").arg("30").spawn().expect("sleep");
            p.pid = other.id();
            std::fs::write(dir.join("dhu.state"), p.render()).expect("state");
            let err = stop(&dir, Some(&adb), Duration::from_secs(1)).expect_err("not the DHU");
            assert!(err.contains("isn't a Desktop Head Unit any more"), "{err}");
            assert!(other.try_wait().expect("wait").is_none(), "the unrelated process still runs");
            let _ = other.kill();
            let _ = other.wait();
            std::fs::remove_dir_all(root).ok();
        }
    }
}
