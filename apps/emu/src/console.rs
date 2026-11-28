//! The emulator console: a line-oriented TCP control channel on the console port, as used by
//! `adb emu <cmd>` and test harnesses. Same framing and replies as the stock console
//! (`OK` / `KO: <reason>`, CRLF line endings, token auth from `~/.emulator_console_auth_token`).
//!
//! It builds everywhere so its commands are tested on any host; only Apple silicon serves it.
#![cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use emu_vmm::Inputs;
use sim_core::frame::encode_rgba;

pub struct ConsoleInfo {
    pub avd_name: String,
    pub stop: Arc<AtomicBool>,
    pub display: Option<Arc<emu_vmm::display::FrameStore>>,
    pub inputs: Arc<std::sync::Mutex<Option<emu_vmm::Inputs>>>,
    pub sensors: Arc<emu_vmm::pipe::sensors::Sensors>,
    pub clipboard: Arc<emu_vmm::pipe::clipboard::Clipboard>,
    pub fingerprint: Arc<emu_vmm::pipe::fingerprint::Fingerprint>,
    pub gps: Arc<emu_vmm::pipe::gps::Gps>,
    pub battery: Arc<emu_vmm::devices::goldfish_battery::Battery>,
    pub telephony: Arc<emu_vmm::telephony::Telephony>,
    /// The `qemud:xr_service` link, on Android XR images.
    pub xr: Option<Arc<emu_vmm::pipe::xr::Xr>>,
    pub car: Option<Arc<emu_vmm::pipe::car::Car>>,
    /// What `network speed|delay` asked for (seeded by `-netspeed`/`-netdelay`). Reported by
    /// `network status` and shared with the guest's NAT, which shapes its traffic by it.
    pub network: Arc<std::sync::Mutex<sim_core::network::Conditions>>,
    /// The tap on the sound card's playback (`audio capture`).
    pub audio: Arc<emu_vmm::audio::tap::Tap>,
    /// A foldable's hinge (`fold`, `unfold`, `posture`).
    pub fold: Option<Arc<crate::fold::Foldable>>,
    /// The guest may turn its display without emu knowing (`crate::instance::rotation_unknown`):
    /// `rotation` says `unknown`.
    pub rotation_unknown: bool,
}

/// Stock's reply to a command group without, or with an unknown, sub-command: the group's
/// description and sub-commands (`dump_help`), then the KO.
fn sub_help(description: &str, subcommands: &[(&str, &str)], ko: &str) -> String {
    let mut s = format!("{description}\r\n\r\navailable sub-commands:\r\n");
    for (name, what) in subcommands {
        s.push_str(&format!("    {name:<15}  {what}\r\n"));
    }
    s.push_str("\r\n");
    s.push_str(ko);
    s
}

/// `network status|speed <speed>|delay <latency>` (stock syntax and replies; names and
/// values in [`sim_core::network`]). Stock reads the whole rest of the line as the argument.
/// simee adds `network loss <percent>|<up>:<down>|none` (stock has no loss command), which the
/// NAT applies to each direction's IP packets and `network status` reports while set.
fn network(info: &ConsoleInfo, words: &[&str]) -> String {
    const SUBCOMMANDS: &[(&str, &str)] = &[
        ("status", "dump network status"),
        ("speed", "change network speed"),
        ("delay", "change network latency"),
        ("loss", "change network packet loss (simee)"),
    ];
    const ABOUT: &str = "allows you to manage the settings related to the network data connection of the\r\nemulated device.";
    let mut conditions = info.network.lock().unwrap_or_else(|e| e.into_inner());
    match words {
        [] => sub_help(ABOUT, SUBCOMMANDS, "KO: missing sub-command"),
        ["status", ..] => format!("{}\r\nOK", conditions.status()),
        ["speed"] => "KO: missing <speed> argument, see 'help network speed'".into(),
        ["speed", arg @ ..] if conditions.set_speed(&arg.join(" ")) => "OK".into(),
        ["speed", ..] => "KO: invalid <speed> argument, see 'help network speed' for valid values".into(),
        ["delay"] => "KO: missing <delay> argument, see 'help network delay'".into(),
        ["delay", arg @ ..] if conditions.set_latency(&arg.join(" ")) => "OK".into(),
        ["delay", ..] => "KO: invalid <delay> argument, see 'help network delay' for valid values".into(),
        ["loss"] => "KO: missing <loss> argument: network loss <percent> or <up>:<down>, each 0 to 100 (none: no loss)".into(),
        ["loss", arg @ ..] => match conditions.set_loss(&arg.join(" ")) {
            Ok(()) => "OK".into(),
            Err(why) => format!("KO: invalid <loss> argument: {why}; use <percent> or <up>:<down>"),
        },
        _ => sub_help(ABOUT, SUBCOMMANDS, "KO:  bad sub-command"),
    }
}

/// `avd snapshot ...`, answered as stock answers when it can't snapshot: simee's renderer
/// can't save its state yet (docs/roadmap-android.md, "Snapshots"), so `save` and `load` fail
/// with the stock Snapshotter's unsupported-graphics messages, `list` finds none and `del`
/// has nothing to delete (stock's delete of a missing snapshot succeeds). Tools see a `KO`
/// and fall back to app-level state.
fn avd_snapshot(words: &[&str]) -> String {
    const SUBCOMMANDS: &[(&str, &str)] = &[
        ("list", "list available state snapshots"),
        ("save", "save state snapshot"),
        ("load", "load state snapshot"),
        ("del", "delete state snapshot"),
        ("delete", "delete state snapshot"),
    ];
    const ABOUT: &str = "allows you to save and restore the virtual device state in snapshots";
    match words {
        [] => sub_help(ABOUT, SUBCOMMANDS, "KO: missing sub-command"),
        // qemu_listvms with no snapshots; stock ends that line with a bare LF.
        ["list", ..] => "There is no snapshot available.\nOK".into(),
        ["save"] => "KO: Argument missing, try 'avd snapshot save <name>'".into(),
        ["save", ..] => format!(
            "KO: Unable to save snapshot: This feature is not currently supported with your selected graphics settings. ({SNAPSHOT_RENDERER})"
        ),
        ["load"] => "KO: Argument missing, try 'avd snapshot load <name>'".into(),
        ["load", ..] => {
            format!("KO: Unable to load snapshot: Snapshots are not supported with the current graphics settings. ({SNAPSHOT_RENDERER})")
        }
        // Stock's message names `list` here.
        // The names come from `sim_core::console`, which the MCP server reads to ask before a delete.
        [name] if sim_core::console::is_snapshot_delete(name) => "KO: Argument missing, try 'avd snapshot list <name>'".into(),
        [name, ..] if sim_core::console::is_snapshot_delete(name) => "OK".into(),
        _ => sub_help(ABOUT, SUBCOMMANDS, "KO:  bad sub-command"),
    }
}

/// The renderer stock names in its snapshot errors.
const SNAPSHOT_RENDERER: &str = "gfxstream";

/// simee extension (stock has no console command for the vehicle HAL; its car controls live
/// in the UI and gRPC): `car set <property> [<area>] <value>`, `car get <property> [<area>]`,
/// `car list` (every property's current value, from the guest) and `car names`. Properties
/// are VHAL names (`GEAR_SELECTION`, or short `gear`, `speed`, `ignition`, `night`,
/// `parking-brake`) or numeric ids; values are typed by the property (`drive`, `10.5`, `true`).
/// Value lines read `property=<name> id=<hex> area=<n> status=<s> value=<text>`.
fn car(info: &ConsoleInfo, words: &[&str]) -> String {
    use sim_core::vhal;
    let Some(car) = &info.car else { return "KO: not an Automotive image: there is no vehicle HAL".into() };
    let lines = |values: &[vhal::PropValue]| values.iter().map(|v| vhal::describe(v) + "\r\n").collect::<String>() + "OK";
    let result = match words {
        ["set", rest @ ..] => vhal::parse_set(rest).and_then(|v| car.set(v)).map(|()| "OK".to_string()),
        ["get", rest @ ..] => vhal::parse_get(rest).and_then(|(prop, area)| car.get(prop, area)).map(|v| lines(&[v])),
        ["list"] => car.all().map(|values| lines(&values)),
        ["names"] => Ok(vhal::PROPERTIES.iter().map(|(n, id)| format!("{n} {:#010x}\r\n", *id as u32)).collect::<String>() + "OK"),
        _ => Err("usage: car set <property> [<area>] <value> | car get <property> [<area>] | car list | car names".into()),
    };
    result.unwrap_or_else(|e| format!("KO: {e}"))
}

/// `sensor set <name> <v[:v...]>` / `sensor get <name>` (stock syntax).
fn sensor(info: &ConsoleInfo, words: &[&str]) -> String {
    match words {
        ["get", name] => match info.sensors.get(name) {
            Some(v) => format!("{name} = {}\r\nOK", v.iter().map(|x| format!("{x}")).collect::<Vec<_>>().join(":")),
            None => format!("KO: unknown sensor '{name}'"),
        },
        ["set", name, values] => {
            let parsed: Result<Vec<f32>, _> = values.split(':').map(str::parse).collect();
            match parsed {
                // A foldable's hinge moves its posture and screen too, as stock's does.
                Ok(v) if *name == "hinge-angle0" && v.len() == 1 && info.fold.is_some() => {
                    match info.fold.as_deref().map(|f| f.set_angle(v[0])) {
                        Some(Err(e)) => format!("KO: {e}"),
                        _ => "OK".into(),
                    }
                }
                Ok(v) => {
                    if info.sensors.set(name, v) {
                        "OK".into()
                    } else {
                        format!("KO: bad sensor '{name}' or value count")
                    }
                }
                Err(_) => format!("KO: bad values '{values}'"),
            }
        }
        ["status"] => {
            let names: Vec<&str> = emu_vmm::pipe::sensors::NAMES.iter().copied().filter(|n| info.sensors.advertised(n)).collect();
            format!("{}\r\nOK", names.iter().map(|n| format!("{n}: enabled.")).collect::<Vec<_>>().join("\r\n"))
        }
        _ => "KO: usage: sensor set <name> <v:v:v> | sensor get <name> | sensor status".into(),
    }
}

/// A point on the screen as screenshots show it (the guest's layout, turned as the guest rotated
/// its display; a folded foldable's outer screen) as the scanout pixel under it, which the touch
/// screen reports.
fn to_scanout(info: &ConsoleInfo, x: i32, y: i32) -> (i32, i32) {
    info.display.as_ref().map_or((x, y), |d| d.to_scanout(f64::from(x), f64::from(y)))
}

/// The screen's size as screenshots show it: the display's upright frames, or the panel before
/// the first frame.
fn shown_size(info: &ConsoleInfo, panel: (u32, u32)) -> (u32, u32) {
    match info.display.as_ref().map(|d| d.upright_geometry()) {
        Some((w, h, n)) if n > 0 && w > 0 && h > 0 => (w, h),
        _ => panel,
    }
}

/// `event send <type>:<code>:<value> ...` (stock syntax, e.g. `EV_KEY:KEY_POWER:1`,
/// `EV_REL:REL_WHEEL:-1`) and `event text <string>`.
fn event(info: &ConsoleInfo, words: &[&str]) -> String {
    use emu_vmm::virtio::input::{EV_ABS, EV_KEY, EV_REL, EV_SYN, InputEvent};
    use sim_core::goldfish::keycode;
    use sim_core::hid::{keystroke, linux_keycode};
    let guard = info.inputs.lock().unwrap_or_else(|e| e.into_inner());
    let Some(inputs) = guard.as_ref() else { return "KO: no input devices".into() };
    match words.first().copied() {
        Some("send") => {
            let mut batches: Vec<(u16, InputEvent)> = Vec::new();
            for spec in &words[1..] {
                let parts: Vec<&str> = spec.split(':').collect();
                let [t, c, v] = parts.as_slice() else { return format!("KO: bad event '{spec}'") };
                let typ = match *t {
                    "EV_SYN" => EV_SYN,
                    "EV_KEY" => EV_KEY,
                    "EV_REL" => EV_REL,
                    "EV_ABS" => EV_ABS,
                    other => match other.parse() {
                        Ok(n) => n,
                        Err(_) => return format!("KO: bad event type '{other}'"),
                    },
                };
                let code = match (typ, *c) {
                    (EV_REL, "REL_WHEEL") => emu_vmm::virtio::input::REL_WHEEL,
                    (EV_KEY, name) => match keycode(name) {
                        Some(k) => k,
                        None => return format!("KO: unknown key '{name}'"),
                    },
                    (_, n) => match n.parse() {
                        Ok(k) => k,
                        Err(_) => return format!("KO: bad code '{n}'"),
                    },
                };
                let Ok(value) = v.parse::<i32>() else { return format!("KO: bad value '{v}'") };
                batches.push((typ, InputEvent::new(typ, code, value)));
            }
            if inputs.touch.is_none() && batches.iter().any(|(typ, _)| *typ == EV_ABS) {
                return NO_TOUCH.into();
            }
            for (typ, ev) in batches {
                let dev = match (typ, &inputs.touch) {
                    (EV_REL, _) => &inputs.rotary,
                    (EV_ABS, Some(touch)) => touch,
                    _ => &inputs.keyboard,
                };
                dev.send(vec![ev, InputEvent::syn()]);
            }
            "OK".into()
        }
        // US layout, through the same key map iOS typing uses, so capitals and symbols type too.
        Some("text") => {
            const SHIFT: u16 = 42;
            for ch in words[1..].join(" ").chars() {
                let Some((stroke, code)) = keystroke(ch).and_then(|k| Some((k, linux_keycode(k.usage)?))) else { continue };
                if stroke.shift {
                    inputs.keyboard.send(vec![InputEvent::new(EV_KEY, SHIFT, 1), InputEvent::syn()]);
                }
                inputs.keyboard.tap_key(code);
                if stroke.shift {
                    inputs.keyboard.send(vec![InputEvent::new(EV_KEY, SHIFT, 0), InputEvent::syn()]);
                }
            }
            "OK".into()
        }
        _ => "KO: usage: event send <type>:<code>:<value> ... | event text <string>".into(),
    }
}

/// `screenshot [path]`: the latest scanout frame as PNG, turned upright as the guest rotated its
/// display (as `adb screencap` is; stock's gRPC `getScreenshot` follows the accelerometer instead,
/// which shows the content sideways when the guest doesn't follow it). Encoded straight from the
/// capture the display stream shares, so a screenshot of a frame already streamed reads nothing
/// back.
fn screenshot(info: &ConsoleInfo, path: Option<&str>) -> String {
    let Some(display) = &info.display else { return "KO: no display (started with -gpu off?)".into() };
    if display.frames() == 0 {
        return "KO: no frame presented yet".into();
    }
    let Some(shot) = display.latest_upright(|c| (c.width, c.height, c.frame, encode_rgba(c.width, c.height, &c.rgba))) else {
        return "KO: the scanout frame couldn't be read".into();
    };
    let (width, height, frame, png) = shot;
    let path = path.map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("simee-{}.png", std::process::id())));
    match png.and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string())) {
        Ok(()) => format!("{} ({}x{}, frame {})\r\nOK", path.display(), width, height, frame),
        Err(e) => format!("KO: {e}"),
    }
}

/// `geo fix <longitude> <latitude> [<altitude> [<satellites> [<velocity>]]]` (stock syntax;
/// velocity in knots).
fn geo(info: &ConsoleInfo, words: &[&str]) -> String {
    let ["fix", rest @ ..] = words else {
        return "KO: usage: geo fix <longitude> <latitude> [<altitude> [<satellites> [<velocity>]]]".into();
    };
    let nums: Result<Vec<f64>, _> = rest.iter().map(|w| w.parse::<f64>()).collect();
    match nums.as_deref() {
        Ok([lon, lat, more @ ..]) if more.len() <= 3 && (-180.0..=180.0).contains(lon) && (-90.0..=90.0).contains(lat) => {
            let fix = emu_vmm::pipe::gps::Fix {
                longitude: *lon,
                latitude: *lat,
                altitude: more.first().copied().unwrap_or(0.0),
                satellites: more.get(1).map_or(8, |s| s.clamp(1.0, 12.0) as u32),
                speed_knots: more.get(2).copied().unwrap_or(0.0),
                course: 0.0,
            };
            info.gps.set_fix(fix);
            "OK".into()
        }
        _ => "KO: bad geo fix arguments (longitude -180..180, latitude -90..90)".into(),
    }
}

/// `power display|ac on|off|status <s>|present true|false|health <h>|capacity <n>` (stock).
fn power(info: &ConsoleInfo, words: &[&str]) -> String {
    use emu_vmm::devices::goldfish_battery::{Health, Status};
    let b = &info.battery;
    match words {
        ["display"] => {
            let s = b.state();
            let status = match s.status {
                Status::Unknown => "Unknown",
                Status::Charging => "Charging",
                Status::Discharging => "Discharging",
                Status::NotCharging => "Not charging",
                Status::Full => "Full",
            };
            let health = match s.health {
                Health::Unknown => "Unknown",
                Health::Good => "Good",
                Health::Overheat => "Overheat",
                Health::Dead => "Dead",
                Health::Overvoltage => "Overvoltage",
                Health::Failure => "Unspecified failure",
                Health::Cold => "Cold",
            };
            format!(
                "AC: {}\r\nstatus: {status}\r\nhealth: {health}\r\npresent: {}\r\ncapacity: {}\r\nOK",
                if s.ac_online { "online" } else { "offline" },
                s.present,
                s.capacity
            )
        }
        ["ac", v @ ("on" | "off")] => {
            b.update(|s| s.ac_online = *v == "on");
            "OK".into()
        }
        ["status", v] => {
            let st = match *v {
                "unknown" => Status::Unknown,
                "charging" => Status::Charging,
                "discharging" => Status::Discharging,
                "not-charging" => Status::NotCharging,
                "full" => Status::Full,
                _ => return "KO: Usage: \"status unknown|charging|discharging|not-charging|full\"".into(),
            };
            b.update(|s| s.status = st);
            "OK".into()
        }
        ["present", v @ ("true" | "false")] => {
            b.update(|s| s.present = *v == "true");
            "OK".into()
        }
        ["health", v] => {
            let h = match *v {
                "unknown" => Health::Unknown,
                "good" => Health::Good,
                "overheat" => Health::Overheat,
                "dead" => Health::Dead,
                "overvoltage" => Health::Overvoltage,
                "failure" => Health::Failure,
                "cold" => Health::Cold,
                _ => return "KO: Usage: \"health unknown|good|overheat|dead|overvoltage|failure|cold\"".into(),
            };
            b.update(|s| s.health = h);
            "OK".into()
        }
        ["capacity", v] => match v.parse::<u32>() {
            Ok(n) if n <= 100 => {
                b.update(|s| s.capacity = n);
                "OK".into()
            }
            _ => "KO: Usage: \"capacity <percentage>\"".into(),
        },
        _ => "KO: Usage: power display|ac on|off|status <state>|present true|false|health <state>|capacity <n>".into(),
    }
}

/// `sms send <phonenumber> <message>` delivers an SMS to the guest (stock syntax); `sms list`
/// (simee) shows what the guest sent.
fn sms(info: &ConsoleInfo, words: &[&str]) -> String {
    match words {
        ["send", from, text @ ..] if !text.is_empty() => match info.telephony.sms_deliver(from, &text.join(" ")) {
            Ok(()) => "OK".into(),
            Err(e) => format!("KO: {e}"),
        },
        ["list"] => {
            let mut s: String = info.telephony.sent_sms().iter().map(|(to, text)| format!("to {to}: {text}\r\n")).collect();
            s.push_str("OK");
            s
        }
        _ => "KO: Usage: sms send <phonenumber> <message>".into(),
    }
}

/// `gsm call|accept|busy|cancel|hold <number>`, `gsm list`, `gsm voice|data <state>`,
/// `gsm status`, `gsm signal <rssi> [<ber>]` (stock syntax).
fn gsm(info: &ConsoleInfo, words: &[&str]) -> String {
    use emu_vmm::modem::RegState;
    use emu_vmm::telephony::CallOp;
    let t = &info.telephony;
    let op = |op: CallOp, n: &str| match t.call(op, n) {
        Ok(()) => "OK".to_string(),
        Err(e) => format!("KO: {e}"),
    };
    match words {
        ["call", n] => op(CallOp::Incoming, n),
        ["accept", n] => op(CallOp::Accept, n),
        ["busy", n] => op(CallOp::Busy, n),
        ["cancel", n] => op(CallOp::Cancel, n),
        ["hold", n] => op(CallOp::Hold, n),
        ["list"] => {
            let mut s: String = t.list_calls().iter().map(|l| format!("{l}\r\n")).collect();
            s.push_str("OK");
            s
        }
        [which @ ("voice" | "data"), state] => match RegState::from_name(state) {
            Some(st) => {
                if *which == "voice" {
                    t.set_voice(st)
                } else {
                    t.set_data(st)
                }
                "OK".into()
            }
            None => "KO: bad state, use unregistered|home|roaming|searching|denied|off|on".into(),
        },
        ["status"] => {
            let (v, d) = t.states();
            format!("gsm voice state: {}\r\ngsm data state:  {}\r\nOK", v.name(), d.name())
        }
        ["signal", rssi, rest @ ..] if rest.len() <= 1 => {
            let ber = rest.first().map_or(Ok(99), |b| b.parse::<u8>());
            match (rssi.parse::<u8>(), ber) {
                (Ok(r), Ok(b)) if (r <= 31 || r == 99) && (b <= 7 || b == 99) => {
                    t.set_signal(r, b);
                    "OK".into()
                }
                _ => "KO: rssi 0..31 (or 99), ber 0..7 (or 99)".into(),
            }
        }
        _ => "KO: Usage: gsm call|accept|busy|cancel|hold <number> | list | voice|data <state> | status | signal <rssi> [<ber>]".into(),
    }
}

/// `xr ...` (simee extension, Android XR images): what the stock emulator's XR controls send the
/// guest over `qemud:xr_service` (stock reaches them only through gRPC), the dual-mode mouse as a
/// pointer at device pixels, and `xr status`. Syntax: [`sim_core::xr::USAGE`].
fn xr(info: &ConsoleInfo, words: &[&str]) -> String {
    use sim_core::xr::{XrCommand, XrPointer, XrResponse, pointer_abs};
    let Some(xr) = &info.xr else { return "KO: not an Android XR device".into() };
    if words == ["status"] {
        let reply = match xr.last_reply() {
            None => "none".to_owned(),
            Some(r) => XrResponse::decode(&r).map_or_else(|| format!("{} undecodable bytes", r.len()), |r| r.to_string()),
        };
        let guest = if xr.connected() { "connected" } else { "not connected" };
        return format!("xr_service: {guest}\r\nlast reply: {reply}\r\nOK");
    }
    let command = match XrCommand::parse(words) {
        Ok(c) => c,
        Err(e) => return format!("KO: {e}"),
    };
    if let XrCommand::Pointer { x, y, phase } = command {
        let guard = info.inputs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(inputs) = guard.as_ref() else { return "KO: no input devices".into() };
        let Some(mouse) = &inputs.mouse else { return "KO: no dual-mode mouse".into() };
        let (width, height) = inputs.screen;
        let (ax, ay) = (pointer_abs(x, width), pointer_abs(y, height));
        match phase {
            XrPointer::Move => mouse.pointer(ax, ay, None),
            XrPointer::Down => mouse.pointer(ax, ay, Some(true)),
            XrPointer::Up => mouse.pointer(ax, ay, Some(false)),
            XrPointer::Click => {
                mouse.pointer(ax, ay, Some(true));
                mouse.pointer(ax, ay, Some(false));
            }
        }
        return "OK".into();
    }
    match command.request() {
        Some(request) if !xr.send(&request) => "KO: the guest's XR service isn't connected (still booting?)".into(),
        _ => "OK".into(),
    }
}

/// `audio capture start <path|unix:socket> [wav|raw]`, `audio capture stop`, `audio capture status`
/// (simee): copies what the guest plays, as the guest wrote it, to a file (a WAV by default) or a
/// Unix socket (raw by default). Works under `-no-audio`. The format is in the WAV header, and in
/// every status reply once the guest has played something.
fn audio(info: &ConsoleInfo, words: &[&str]) -> String {
    use emu_vmm::audio::tap::{Container, Sink, Status};
    const USAGE: &str = "KO: usage: audio capture start <path|unix:socket> [wav|raw] | audio capture stop | audio capture status";
    fn report(s: &Status) -> String {
        let format = s.format.map_or_else(|| "unknown (the guest hasn't played yet)".to_string(), |f| f.to_string());
        let mut out = format!(
            "capture: {}\r\nformat: {format}\r\nframes captured: {}\r\nframes dropped: {}\r\n",
            if s.capturing { "running" } else { "stopped" },
            s.captured_frames,
            s.dropped_frames
        );
        if let Some(e) = &s.error {
            out.push_str(&format!("error: {e}\r\n"));
        }
        out.push_str("OK");
        out
    }
    let tap = &info.audio;
    match words {
        ["capture", "start", target, rest @ ..] if rest.len() <= 1 => {
            if !tap.status().card {
                return "KO: this device has no sound card to capture".into();
            }
            let (socket, path) = match target.strip_prefix("unix:") {
                Some(p) => (true, p),
                None => (false, *target),
            };
            let container = match (rest.first().copied(), socket) {
                (Some("wav"), _) | (None, false) => Container::Wav,
                (Some("raw"), _) | (None, true) => Container::Raw,
                _ => return USAGE.into(),
            };
            // Opened only once the tap knows no capture is running: a refused start leaves the
            // running capture's file alone.
            let open = || {
                let sink = if socket {
                    sim_core::net::UnixStream::connect(path).map(|s| {
                        // A reader that stops reading drops frames (counted); `stop` waits for it at most a couple of seconds.
                        let _ = s.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                        Sink::Stream(Box::new(s))
                    })
                } else {
                    std::fs::File::create(path).map(|f| Sink::Seekable(Box::new(f)))
                };
                sink.map_err(|e| format!("cannot open {path}: {e}"))
            };
            match tap.start_with(open, container) {
                Ok(()) => "OK".into(),
                Err(e) => format!("KO: {e}"),
            }
        }
        ["capture", "stop"] => match tap.stop() {
            Some(s) => report(&s),
            None => "KO: no capture is running".into(),
        },
        ["capture", "status"] => report(&tap.status()),
        _ => USAGE.into(),
    }
}

const NO_TOUCH: &str = "KO: this device has no touch screen (hw.screen=no-touch)";

/// Commands this console implements (stock names first, then simee extensions).
const HELP: &[(&str, &str)] = &[
    ("help|h|?", "print a list of commands"),
    ("kill", "kill the emulator instance"),
    ("quit|exit", "quit control session"),
    ("event", "simulate hardware events"),
    ("avd", "control virtual device execution"),
    ("finger", "manage emulator fingerprint"),
    ("geo", "Geo-location commands"),
    ("power", "power related commands"),
    ("rotate", "rotate the screen clockwise by 90 degrees"),
    ("fold", "fold the device"),
    ("unfold", "unfold the device"),
    ("posture", "change posture of foldable device"),
    ("sms", "SMS related commands"),
    ("gsm", "GSM related commands"),
    ("network", "manage network settings"),
    ("audio", "capture what the guest plays: capture start|stop|status (simee)"),
    ("ping", "check if the emulator is alive"),
    ("sensor", "manage emulator sensors"),
    ("screenshot", "take a screenshot of the display"),
    ("rotation", "degrees the guest rotated its display, which screenshots and touches follow (simee)"),
    ("touch", "touch the screen at x y (simee)"),
    ("touchpad", "touch the touchpad at x y, display glasses (simee)"),
    ("pen", "a stylus on the screen at x y: hover, down, move, up, leave (simee)"),
    ("clipboard", "get or set the guest clipboard (simee)"),
    ("frames", "number of frames presented (simee)"),
    ("xr", "Android XR head pose, room, input mode and pointer (simee)"),
    ("car", "get or set vehicle HAL properties on Automotive images (simee)"),
    ("camera", "list or relink the back and front cameras' sources (simee)"),
];

/// `camera list` (each camera's source) and `camera link <back|front> <source>`: a relink takes
/// effect on the guest's next frame, at the size its camera is open at.
fn camera(args: &[&str]) -> String {
    use emu_vmm::pipe::camera::{Cameras, Facing, parse_link};
    let cameras = Cameras::global();
    match args {
        ["list"] => {
            let lines: Vec<String> = Facing::ALL
                .iter()
                .map(|&f| format!("{}: {}", f.dir(), cameras.linked(f).map_or("none".to_string(), |s| s.to_string())))
                .collect();
            format!("{}\r\nOK", lines.join("\r\n"))
        }
        ["link", facing, source @ ..] if !source.is_empty() => {
            let Some(facing) = Facing::parse(facing) else { return "KO: no such camera (back or front)".into() };
            match parse_link(&source.join(" ")) {
                Ok(link) => {
                    cameras.link(facing, link);
                    "OK".into()
                }
                Err(e) => format!("KO: {e}"),
            }
        }
        _ => "KO: usage: camera list | camera link back|front <source id>|emulated|none".into(),
    }
}

fn token_path() -> Option<PathBuf> {
    // `HOME` on unix, `USERPROFILE` on Windows, as the stock emulator has it.
    std::env::home_dir().map(|h| h.join(".emulator_console_auth_token"))
}

/// The auth token, creating one like the stock emulator does when the file is missing.
/// An empty file disables authentication.
fn auth_token() -> Option<String> {
    token_at(&token_path()?)
}

/// [`auth_token`] for the file at `path`.
fn token_at(path: &std::path::Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Some(t.trim().to_string()).filter(|t| !t.is_empty()),
        Err(_) => {
            let token = sim_core::entropy::hex(12).ok()?[..16].to_string();
            std::fs::write(path, &token).ok()?;
            Some(token)
        }
    }
}

pub fn serve(port: u16, info: ConsoleInfo) -> Result<()> {
    // Before any client can connect, as stock makes it at startup: a client reads the token file
    // first, and one made by its own connection would leave it unauthenticated (every command
    // then answers `KO: unknown command`).
    let _ = auth_token();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| format!("binding console port {port}"))?;
    let info = Arc::new(info);
    std::thread::Builder::new().name("console".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let info = info.clone();
            let _ = std::thread::Builder::new().name("console-session".into()).spawn(move || {
                let _ = session(stream, &info);
            });
        }
    })?;
    Ok(())
}

fn session(stream: TcpStream, info: &ConsoleInfo) -> std::io::Result<()> {
    // Replies are small and the client waits for each: no Nagle delay behind a delayed ACK.
    stream.set_nodelay(true)?;
    let mut out = stream.try_clone()?;
    let token = auth_token();
    let mut authed = token.is_none();
    if authed {
        out.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n")?;
    } else {
        let path = token_path().map(|p| p.display().to_string()).unwrap_or_default();
        out.write_all(
            format!(
                "Android Console: Authentication required\r\nAndroid Console: type 'auth <auth_token>' to authenticate\r\n\
                 Android Console: you can find your <auth_token> in \r\n'{path}'\r\nOK\r\n"
            )
            .as_bytes(),
        )?;
    }
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = words.first() else { continue };
        let reply = match (cmd, authed) {
            ("auth", _) => {
                if words.get(1).copied() == token.as_deref() || token.is_none() {
                    authed = true;
                    "Android Console: type 'help' for a list of commands\r\nOK".to_string()
                } else {
                    "KO: authentication token does not match ~/.emulator_console_auth_token".to_string()
                }
            }
            // Stock closes without a reply; `adb emu <cmd>` sends `quit` after the command and
            // prints everything it reads, so an extra OK leaks into answers like `avd name`.
            ("quit" | "exit", _) => return Ok(()),
            (_, false) => "KO: unknown command, try 'help'".to_string(),
            (kill, true) if kill == sim_core::console::KILL => {
                out.write_all(b"OK: killing emulator, bye bye\r\n")?;
                // While the guest still runs: a folded foldable's forced size goes with emu.
                if let Some(fold) = &info.fold {
                    fold.release();
                }
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                emu_vmm::request_stop(&info.stop);
                #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
                info.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                return Ok(());
            }
            (_, true) => command(info, &words),
        };
        let mut reply = reply;
        reply.push_str("\r\n");
        out.write_all(reply.as_bytes())?;
    }
    Ok(())
}

/// One authenticated command line (split into words), answered as stock does: the reply's lines
/// ending in `OK`, or `KO: <reason>`. `auth`, `quit` and `kill` belong to the session.
fn command(info: &ConsoleInfo, words: &[&str]) -> String {
    let Some(&cmd) = words.first() else { return String::new() };
    match cmd {
        // Lists what simee implements, in stock's format (tools probe it, e.g. for "finger").
        "help" | "h" | "?" => {
            let mut s = String::from("Android console commands:\r\n\r\n");
            for (name, what) in HELP {
                s.push_str(&format!("    {name:<16} {what}\r\n"));
            }
            s.push_str("\r\ntry 'help <command>' for command-specific help\r\nOK");
            s
        }
        "ping" => "I am alive!\r\nOK".to_string(),
        "screenshot" => screenshot(info, words.get(1).copied()),
        "event" => event(info, &words[1..]),
        "sensor" => sensor(info, &words[1..]),
        "geo" => geo(info, &words[1..]),
        "power" => power(info, &words[1..]),
        "sms" => sms(info, &words[1..]),
        "gsm" => gsm(info, &words[1..]),
        "xr" => xr(info, &words[1..]),
        "car" => car(info, &words[1..]),
        "network" => network(info, &words[1..]),
        "audio" => audio(info, &words[1..]),
        "fold" if words.get(1) == Some(&"status") => crate::fold::status(info.fold.as_deref()),
        "fold" | "unfold" | "posture" => crate::fold::console(info.fold.as_deref(), words),
        // Stock: rotate the device 90° clockwise. Gravity in device coordinates turns the
        // other way, and the guest's auto-rotation follows the accelerometer if it will; the
        // frames turn when the guest reports that it rotated (`crate::instance`'s logcat watch).
        "rotate" => match info.sensors.get("acceleration").as_deref() {
            Some(&[x, y, z]) => {
                info.sensors.set("acceleration", vec![-y + 0.0, x + 0.0, z]);
                info.sensors.set("acceleration-uncalibrated", vec![-y + 0.0, x + 0.0, z]);
                "OK".into()
            }
            _ => "KO: no accelerometer".into(),
        },
        "finger" => match (words.get(1).copied(), words.get(2).and_then(|v| v.parse::<u32>().ok())) {
            (Some("touch"), Some(id)) if id > 0 => {
                info.fingerprint.touch(id);
                "OK".into()
            }
            (Some("remove"), _) => {
                info.fingerprint.remove();
                "OK".into()
            }
            _ => "KO: usage: finger touch <fingerid> | finger remove".into(),
        },
        // simee extension: `camera list` / `camera link back|front <source id>|emulated|none`.
        "camera" => camera(&words[1..]),
        // simee extension: `clipboard get` / `clipboard set <text>`.
        "clipboard" => match words.get(1).copied() {
            Some("get") => format!("{}\r\nOK", String::from_utf8_lossy(&info.clipboard.get())),
            Some("set") => {
                info.clipboard.set(words[2..].join(" ").as_bytes());
                "OK".into()
            }
            _ => "KO: usage: clipboard get|set <text>".into(),
        },
        // simee extension: how far the frames are turned clockwise to show the guest's display
        // upright, in degrees (the guest's own rotation): what `touch`, `pen` and screenshots
        // are turned by.
        // `unknown` where emu doesn't read the guest's rotation (API 29 and 30): frames and
        // touches stay in panel space there, whichever way the guest turned.
        "rotation" => match &info.display {
            _ if info.rotation_unknown => "rotation: unknown\r\nOK".into(),
            Some(d) => format!("rotation: {}\r\nOK", d.rotation().degrees()),
            None => "rotation: 0\r\nOK".into(),
        },
        "touch" => {
            // simee extension: `touch <x> <y> [down|up|tap]` in pixels of the screen as
            // screenshots show it.
            let g = info.inputs.lock().unwrap_or_else(|e| e.into_inner());
            match (g.as_ref(), words.get(1).and_then(|v| v.parse().ok()), words.get(2).and_then(|v| v.parse().ok())) {
                (Some(Inputs { touch: None, .. }), _, _) => NO_TOUCH.to_string(),
                (Some(Inputs { touch: Some(touch), .. }), Some(x), Some(y)) => {
                    let (x, y) = to_scanout(info, x, y);
                    match words.get(3).copied().unwrap_or("tap") {
                        "down" => touch.touch(x, y, true),
                        "up" => touch.touch(x, y, false),
                        _ => {
                            touch.touch(x, y, true);
                            touch.touch(x, y, false);
                        }
                    }
                    "OK".to_string()
                }
                _ => "KO: usage: touch <x> <y> [down|up|tap]".to_string(),
            }
        }
        // simee extension: `touchpad <x> <y> [down|up|tap]` in touchpad units (display glasses).
        "touchpad" => {
            let g = info.inputs.lock().unwrap_or_else(|e| e.into_inner());
            match (g.as_ref(), words.get(1).and_then(|v| v.parse().ok()), words.get(2).and_then(|v| v.parse().ok())) {
                (Some(Inputs { touchpad: None, .. }), _, _) => "KO: this device has no touchpad (hw.touchpad0)".to_string(),
                (Some(Inputs { touchpad: Some(pad), .. }), Some(x), Some(y)) => match words.get(3).copied().unwrap_or("tap") {
                    "down" | "move" => {
                        pad.touch(x, y, true);
                        "OK".to_string()
                    }
                    "up" => {
                        pad.touch(x, y, false);
                        "OK".to_string()
                    }
                    "tap" => {
                        pad.touch(x, y, true);
                        pad.touch(x, y, false);
                        "OK".to_string()
                    }
                    other => format!("KO: bad phase '{other}' (down, move, up, tap)"),
                },
                _ => "KO: usage: touchpad <x> <y> [down|move|up|tap]".to_string(),
            }
        }
        // simee extension: a stylus on the touch screen at pixels of the screen as screenshots
        // show it, what stock's gRPC `PenEvent` sends (`sim_core::stylus::USAGE`).
        "pen" => {
            let g = info.inputs.lock().unwrap_or_else(|e| e.into_inner());
            let Some(inputs) = g.as_ref() else { return "KO: no input devices".into() };
            let Some(touch) = &inputs.touch else { return NO_TOUCH.into() };
            let (w, h) = shown_size(info, inputs.screen);
            match sim_core::stylus::PenSample::parse(&words[1..]) {
                Ok(s) if s.x as u32 >= w || s.y as u32 >= h => format!("KO: ({}, {}) is off the {w}x{h} screen", s.x, s.y),
                Ok(s) => {
                    let (x, y) = to_scanout(info, s.x, s.y);
                    touch.pen(sim_core::stylus::PenSample { x, y, ..s });
                    "OK".into()
                }
                Err(e) => format!("KO: {e}"),
            }
        }
        "frames" => match &info.display {
            Some(d) => format!("{}\r\nOK", d.frames()),
            None => "KO: no display".to_string(),
        },
        "avd" => match words.get(1).copied() {
            Some("name") => format!("{}\r\nOK", info.avd_name),
            Some("status") => "virtual device is running\r\nOK".to_string(),
            Some("id") => format!("{}\r\nOK", info.avd_name),
            Some("snapshot") => avd_snapshot(&words[2..]),
            _ => "KO: unsupported 'avd' subcommand in simee".to_string(),
        },
        other => format!("KO: '{other}' is not implemented in simee yet"),
    }
}

/// `<discovery dir>/avd/running/pid_<pid>.ini`, which tools read to find running instances.
pub struct Advertisement {
    path: PathBuf,
}

impl Advertisement {
    pub fn write(avd_name: &str, avd_dir: &std::path::Path, console_port: u16) -> Option<Advertisement> {
        let dir = emu_avd::running_dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        prune_stale(&dir);
        let path = dir.join(format!("pid_{}.ini", std::process::id()));
        let cmdline: Vec<String> = std::env::args().collect();
        let body = format!(
            "port.serial={console_port}\nport.adb={}\navd.name={avd_name}\navd.id={avd_name}\navd.dir={}\ncmdline=\"{}\"\nemulator=simee\n",
            console_port + 1,
            avd_dir.display(),
            cmdline.join("\" \"")
        );
        std::fs::write(&path, body).ok()?;
        Some(Advertisement { path })
    }
}

/// Removes simee advertisements whose process is gone (a teardown that hit the hard-exit
/// timer skips the Drop below). Stale entries claim ports and confuse discovery tools.
/// Stock emulator files are left alone.
fn prune_stale(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_owned) else { continue };
        let Some(running) = std::fs::read_to_string(e.path()).ok().and_then(|t| emu_avd::Running::parse(&name, &t)) else { continue };
        if running.simee && !running.alive() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    use emu_vmm::pipe::car::CarService;
    use emu_vmm::pipe::xr::XrService;
    use emu_vmm::pipe::{HostMsg, PipeCtx, PipeService, WorkerEvent};
    use emu_vmm::virtio::input::{EV_KEY, EV_REL, Input, InputEvent, Kind, REL_WHEEL};
    use sim_core::form_factor::FormFactor;
    use sim_core::vhal::{GEAR_SELECTION, Message, MsgType, PropValue};

    use super::*;

    /// A console as a device of `form_factor` gets it: its sensors, and the XR link or the
    /// vehicle HAL when the image has one.
    fn console(form_factor: FormFactor) -> ConsoleInfo {
        let mask = sim_core::goldfish::sensor_mask(|_| None, form_factor, Some(34));
        let sensors = Arc::new(emu_vmm::pipe::sensors::Sensors::new(mask));
        let fold = (form_factor == FormFactor::Foldable)
            .then(|| Arc::new(crate::fold::Foldable::new(sim_core::fold::pixel_fold().hinge, (2208, 1840), sensors.clone(), true)));
        ConsoleInfo {
            avd_name: "Test_AVD".into(),
            stop: Arc::default(),
            display: None,
            inputs: Arc::default(),
            sensors,
            clipboard: Arc::default(),
            fingerprint: Arc::default(),
            gps: Arc::default(),
            battery: Arc::default(),
            telephony: Arc::default(),
            xr: (form_factor == FormFactor::Vision).then(Arc::default),
            car: (form_factor == FormFactor::Auto).then(Arc::default),
            network: Arc::default(),
            audio: Arc::default(),
            fold,
            rotation_unknown: false,
        }
    }

    struct Devices {
        touch: Option<Input>,
        keyboard: Input,
        rotary: Input,
        mouse: Option<Input>,
    }

    /// Attaches input devices (a 720x1280 touch screen unless `no_touch`; the dual-mode mouse
    /// on request) that no guest driver has taken, so the events a command sends stay queued.
    fn attach_inputs(info: &ConsoleInfo, with_mouse: bool) -> Devices {
        attach(info, with_mouse, true)
    }

    fn attach(info: &ConsoleInfo, with_mouse: bool, touch_screen: bool) -> Devices {
        let (touch, th) = if touch_screen {
            let (t, h) = Input::new(Kind::Touch { width: 720, height: 1280, screen: true }, "touch");
            (Some(t), Some(h))
        } else {
            (None, None)
        };
        let (keyboard, kh) = Input::new(Kind::Keyboard, "keyboard");
        let (rotary, rh) = Input::new(Kind::Rotary, "rotary");
        let (mouse, mh) = if with_mouse {
            let (m, h) = Input::new(Kind::DualModeMouse, "mouse");
            (Some(m), Some(h))
        } else {
            (None, None)
        };
        *info.inputs.lock().expect("inputs") =
            Some(Inputs { screen: (720, 1280), touch: th, keyboard: kh, rotary: rh, mouse: mh, touchpad: None });
        Devices { touch, keyboard, rotary, mouse }
    }

    fn run(info: &ConsoleInfo, line: &str) -> String {
        command(info, &line.split_whitespace().collect::<Vec<_>>())
    }

    /// The golden build stops its child through the console like `adb emu kill` does: greeting,
    /// `auth`, `kill`, and the instance's stop flag is set.
    #[test]
    fn the_golden_builds_console_stop_authenticates_and_kills() {
        use std::sync::atomic::Ordering;
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("free port").local_addr().expect("addr").port();
        let info = console(FormFactor::Phone);
        let stop = info.stop.clone();
        serve(port, info).expect("serve");
        let token = auth_token().expect("token");
        assert!(crate::golden::console_kill_with(port, Some("wrong")).is_err(), "a wrong token is refused");
        assert!(!stop.load(Ordering::SeqCst));
        crate::golden::console_kill_with(port, Some(&token)).expect("kill");
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(stop.load(Ordering::SeqCst), "the console's kill requested a stop");
    }

    /// The token exists, as stock's does, before a client reads it: a missing file gets one (16 hex
    /// digits), an existing one is kept, and an empty one turns authentication off.
    #[test]
    fn the_auth_token_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("emu-console-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(".emulator_console_auth_token");
        let _ = std::fs::remove_file(&path);
        let made = token_at(&path).expect("made");
        assert!(made.len() == 16 && made.bytes().all(|b| b.is_ascii_hexdigit()), "{made}");
        assert_eq!(std::fs::read_to_string(&path).expect("written"), made);
        assert_eq!(token_at(&path).as_deref(), Some(made.as_str()), "kept");
        std::fs::write(&path, "").expect("empty");
        assert_eq!(token_at(&path), None, "an empty file turns authentication off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Round trips through the real console server, as `adb emu` and simee make them: one TCP
    /// session per call, pipelined commands, then `quit`.
    ///     cargo test --release -p simee-emu console_latency -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn console_latency() {
        use std::io::Read;
        use std::time::Instant;
        let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe").local_addr().expect("addr").port();
        serve(port, console(FormFactor::Phone)).expect("serve");
        let auth = auth_token().map(|t| format!("auth {t}\r\n")).unwrap_or_default();
        for commands in [1usize, 10] {
            let script = format!("{auth}{}quit\r\n", "ping\r\n".repeat(commands));
            let n = 50;
            let start = Instant::now();
            for _ in 0..n {
                let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
                s.write_all(script.as_bytes()).expect("write");
                let mut out = String::new();
                s.read_to_string(&mut out).expect("read");
                assert_eq!(out.matches("I am alive!").count(), commands);
            }
            println!(
                "console: {commands:>2} command(s) per session: {:.2} ms per session",
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn audio_capture_starts_stops_and_reports_the_format() {
        let info = console(FormFactor::Phone);
        let usage = "KO: usage: audio capture start <path|unix:socket> [wav|raw] | audio capture stop | audio capture status";
        let path = std::env::temp_dir().join(format!("emu-audio-capture-{}.wav", std::process::id()));
        let start = format!("audio capture start {}", path.display());
        assert_eq!(run(&info, &start), "KO: this device has no sound card to capture");
        info.audio.set_card(true);
        assert_eq!(run(&info, "audio capture stop"), "KO: no capture is running");
        assert!(run(&info, "audio capture status").starts_with("capture: stopped\r\nformat: unknown"));
        for round in 0..2 {
            assert_eq!(run(&info, &start), "OK", "round {round}");
            assert!(run(&info, &start).starts_with("KO: already capturing"));
            assert!(run(&info, "audio capture status").starts_with("capture: running\r\n"));
            let pcm = emu_vmm::audio::tap::PcmFormat { rate: 44_100, channels: 1, format: emu_vmm::audio::Format::S16 };
            info.audio.offer(pcm, &[1, 0, 2, 0]);
            let stopped = run(&info, "audio capture stop");
            assert_eq!(
                stopped, "capture: stopped\r\nformat: 44100 Hz 1 ch s16\r\nframes captured: 2\r\nframes dropped: 0\r\nOK",
                "round {round}"
            );
            let wav = std::fs::read(&path).expect("capture file");
            assert_eq!(wav, [emu_vmm::audio::tap::wav_header(pcm, 4), vec![1, 0, 2, 0]].concat());
        }
        let _ = std::fs::remove_file(&path);
        // A refused second start leaves the running capture's file alone.
        {
            assert_eq!(run(&info, &start), "OK");
            info.audio
                .offer(emu_vmm::audio::tap::PcmFormat { rate: 8_000, channels: 1, format: emu_vmm::audio::Format::S16 }, &[1, 0, 2, 0]);
            while info.audio.status().captured_frames < 2 {
                std::thread::yield_now();
            }
            assert_eq!(run(&info, &start), "KO: already capturing; stop it first");
            run(&info, "audio capture stop");
            let wav = std::fs::read(&path).expect("capture file");
            assert_eq!(wav.len(), 44 + 4, "the first capture's samples survived the refused start");
            let _ = std::fs::remove_file(&path);
        }
        assert!(run(&info, "audio capture start /nonexistent-dir/x.wav").starts_with("KO: cannot open /nonexistent-dir/x.wav: "));
        assert_eq!(run(&info, "audio capture start x.wav mp3"), usage);
        assert_eq!(run(&info, "audio"), usage);
    }

    #[test]
    fn help_lists_every_command_dispatched() {
        let info = console(FormFactor::Phone);
        let help = run(&info, "help");
        assert!(help.starts_with("Android console commands:\r\n") && help.ends_with("OK"), "{help}");
        for (names, _) in HELP {
            let name = names.split('|').next().expect("name");
            assert!(help.contains(names), "{name} missing from help");
            if !["kill", "quit"].contains(&name) {
                assert!(!run(&info, name).contains("is not implemented"), "{name} is listed but not dispatched");
            }
        }
        assert_eq!(run(&info, "ping"), "I am alive!\r\nOK");
        assert_eq!(run(&info, "avd name"), "Test_AVD\r\nOK");
        assert_eq!(run(&info, "redir list"), "KO: 'redir' is not implemented in simee yet");
    }

    /// AutoMobile's `setDeviceState --networkCondition`: a profile's delay and speed, the
    /// `network status` readback, then the reset to `none`/`full`.
    #[test]
    fn network_speed_and_delay_answer_as_stock_and_show_in_status() {
        let info = console(FormFactor::Phone);
        let unshaped = "Current network status:\r\n  download speed:          0 bits/s (0.0 KB/s)\r\n  upload speed:            0 bits/s (0.0 KB/s)\r\n  minimum latency:  0 ms\r\n  maximum latency:  0 ms\r\nOK";
        assert_eq!(run(&info, "network status"), unshaped);
        assert_eq!(run(&info, "network delay umts"), "OK");
        assert_eq!(run(&info, "network speed umts"), "OK");
        assert_eq!(
            run(&info, "network status"),
            "Current network status:\r\n  download speed:     384000 bits/s (46.9 KB/s)\r\n  upload speed:       384000 bits/s (46.9 KB/s)\r\n  minimum latency:  35 ms\r\n  maximum latency:  200 ms\r\nOK"
        );
        assert_eq!(run(&info, "network speed 14.4:80"), "OK");
        assert!(
            run(&info, "network status").contains("  download speed:      80000 bits/s (9.8 KB/s)\r\n  upload speed:        14400 bits/s")
        );
        assert_eq!(run(&info, "network speed warp"), "KO: invalid <speed> argument, see 'help network speed' for valid values");
        assert_eq!(run(&info, "network delay slow"), "KO: invalid <delay> argument, see 'help network delay' for valid values");
        assert!(run(&info, "network status").contains("minimum latency:  35 ms"), "a rejected argument changes nothing");
        assert_eq!(run(&info, "network speed"), "KO: missing <speed> argument, see 'help network speed'");
        assert_eq!(run(&info, "network delay"), "KO: missing <delay> argument, see 'help network delay'");
        assert_eq!(run(&info, "network delay none"), "OK");
        assert_eq!(run(&info, "network speed full"), "OK");
        assert_eq!(run(&info, "network status"), unshaped);
        assert!(!info.network.lock().expect("network").is_shaped());

        let missing = run(&info, "network");
        assert!(missing.starts_with("allows you to manage the settings related to the network data connection of the\r\n"), "{missing}");
        assert!(missing.contains("\r\navailable sub-commands:\r\n    status           dump network status\r\n"), "{missing}");
        assert!(missing.ends_with("\r\n\r\nKO: missing sub-command"), "{missing}");
        assert!(run(&info, "network warp").ends_with("\r\nKO:  bad sub-command"));
    }

    /// simee's `network loss <percent>|<up>:<down>|none`: shown in `network status` while set,
    /// its errors say what is wrong and what it takes, and a refused argument changes nothing.
    #[test]
    fn network_loss_sets_each_direction_and_shows_in_status() {
        let info = console(FormFactor::Phone);
        assert_eq!(run(&info, "network loss 10:25"), "OK");
        let status = run(&info, "network status");
        assert!(status.ends_with("  maximum latency:  0 ms\r\n  download loss:    25 %\r\n  upload loss:      10 %\r\nOK"), "{status}");
        let c = *info.network.lock().expect("network");
        assert_eq!((c.upload_loss_pct, c.download_loss_pct), (10.0, 25.0));
        assert_eq!(run(&info, "network loss 5"), "OK");
        assert!(run(&info, "network status").contains("download loss:    5 %\r\n  upload loss:      5 %"));
        assert_eq!(
            run(&info, "network loss"),
            "KO: missing <loss> argument: network loss <percent> or <up>:<down>, each 0 to 100 (none: no loss)"
        );
        assert_eq!(
            run(&info, "network loss 120"),
            "KO: invalid <loss> argument: `120` is out of range, each loss is 0 to 100 percent; use <percent> or <up>:<down>"
        );
        assert_eq!(run(&info, "network loss half"), "KO: invalid <loss> argument: `half` isn't a percentage; use <percent> or <up>:<down>");
        assert!(run(&info, "network status").contains("upload loss:      5 %"), "a rejected argument changes nothing");
        // Speed and delay leave it alone; `none` clears it, and the report is stock's again.
        assert_eq!(run(&info, "network speed full"), "OK");
        assert_eq!(run(&info, "network delay none"), "OK");
        assert!(info.network.lock().expect("network").is_shaped());
        assert_eq!(run(&info, "network loss none"), "OK");
        assert!(!run(&info, "network status").contains("loss"));
        assert!(!info.network.lock().expect("network").is_shaped());
        assert!(run(&info, "network").contains("\r\n    loss             change network packet loss (simee)\r\n"));
    }

    /// Stock's replies when it can't snapshot: every `save`/`load` is a KO a tool can fall
    /// back on, nothing is listed, and deleting succeeds with nothing to delete.
    #[test]
    fn avd_snapshot_answers_as_stock_does_without_snapshot_support() {
        let info = console(FormFactor::Phone);
        assert_eq!(
            run(&info, "avd snapshot save am_capture"),
            "KO: Unable to save snapshot: This feature is not currently supported with your selected graphics settings. (gfxstream)"
        );
        assert_eq!(
            run(&info, "avd snapshot load am_capture"),
            "KO: Unable to load snapshot: Snapshots are not supported with the current graphics settings. (gfxstream)"
        );
        // Every name the MCP server asks about before a delete is one this console deletes by.
        for name in sim_core::console::SNAPSHOT_DELETE {
            assert_eq!(run(&info, &format!("avd snapshot {name} am_capture")), "OK", "{name}");
        }
        assert_eq!(run(&info, "avd snapshot del am_capture"), "OK");
        assert_eq!(run(&info, "avd snapshot delete am_capture"), "OK");
        assert_eq!(run(&info, "avd snapshot list"), "There is no snapshot available.\nOK");
        assert_eq!(run(&info, "avd snapshot save"), "KO: Argument missing, try 'avd snapshot save <name>'");
        assert_eq!(run(&info, "avd snapshot load"), "KO: Argument missing, try 'avd snapshot load <name>'");
        assert_eq!(run(&info, "avd snapshot del"), "KO: Argument missing, try 'avd snapshot list <name>'");
        let missing = run(&info, "avd snapshot");
        assert!(missing.starts_with("allows you to save and restore the virtual device state in snapshots\r\n"), "{missing}");
        assert!(missing.ends_with("KO: missing sub-command"), "{missing}");
        assert!(run(&info, "avd snapshot remap 0").ends_with("KO:  bad sub-command"));
        assert_eq!(run(&info, "avd pause"), "KO: unsupported 'avd' subcommand in simee", "other avd subcommands are unchanged");
    }

    #[test]
    fn rotate_turns_gravity_clockwise_and_back_after_four() {
        let info = console(FormFactor::Phone);
        assert_eq!(run(&info, "rotate"), "OK");
        assert_eq!(run(&info, "sensor get acceleration"), "acceleration = -9.80665:0:0\r\nOK");
        for _ in 0..3 {
            assert_eq!(run(&info, "rotate"), "OK");
        }
        assert_eq!(run(&info, "sensor get acceleration"), "acceleration = 0:9.80665:0\r\nOK");
    }

    /// The accelerometer alone never turns frames or touches: the guest decides whether its
    /// display follows it (auto-rotate off, a portrait-locked app and a phone's 180° leave it as
    /// it is), and the display turns only when the guest reports a rotation (the logcat watch in
    /// `crate::instance`). A device lying flat, with noise, stays as it is.
    #[test]
    fn rotate_and_gravity_alone_dont_turn_the_display() {
        use sim_core::rotation::Rotation;
        for form_factor in FormFactor::ALL {
            let mut info = console(form_factor);
            let store = Arc::new(emu_vmm::display::FrameStore::default());
            info.display = Some(store.clone());
            for _ in 0..4 {
                assert_eq!(run(&info, "rotate"), "OK");
                assert_eq!(store.rotation(), Rotation::Deg0, "{form_factor:?}");
            }
            if info.sensors.get("acceleration").is_some() {
                for gravity in ["0.3:0.1:9.8", "-0.4:0.2:-9.8", "-9.81:0:0"] {
                    assert_eq!(run(&info, &format!("sensor set acceleration {gravity}")), "OK");
                    assert_eq!(store.rotation(), Rotation::Deg0, "{form_factor:?} {gravity}");
                }
            }
        }
    }

    /// The guest kept its display upright (auto-rotate off) while `rotate` turned the
    /// accelerometer: a touch and a pen at uiautomator's bounds press that panel pixel, unturned.
    #[test]
    fn touches_follow_the_guests_rotation_not_the_accelerometer() {
        use emu_vmm::display::DisplaySink;
        use emu_vmm::virtio::input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, EV_ABS};
        let mut info = console(FormFactor::Phone);
        let store = Arc::new(emu_vmm::display::FrameStore::default());
        info.display = Some(store.clone());
        let devices = attach_inputs(&info, false);
        store.present(720, 1280, 720 * 4, sim_core::pixels::PixelFormat::RGBA_VIRTIO, &vec![0u8; 720 * 1280 * 4]);
        assert_eq!(run(&info, "rotate"), "OK");
        assert_eq!(run(&info, "touch 540 1200 down"), "OK");
        assert_eq!(run(&info, "pen 540 1200 down"), "OK");
        let at = |batch: &[InputEvent]| {
            let v = |code| batch.iter().find(|e| e.typ == EV_ABS && e.code == code).map(|e| e.value);
            (v(ABS_MT_POSITION_X), v(ABS_MT_POSITION_Y))
        };
        let batches = devices.touch.expect("touch").take_queued();
        assert_eq!(batches.len(), 2);
        // (540, 1200) of the 720x1280 panel on the touch screen's 0..32768 axes.
        for b in &batches {
            assert_eq!(at(b), (Some(540 * 32768 / 720), Some(1200 * 32768 / 1280)), "{b:?}");
        }
    }

    /// The guest rotated its display a quarter (its layout 1280x720): screenshots, touches and
    /// pens are in that layout's pixels.
    #[test]
    fn a_turned_devices_screenshots_touches_and_pens_are_upright() {
        use emu_vmm::display::DisplaySink;
        use emu_vmm::virtio::input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, EV_ABS};
        let mut info = console(FormFactor::Tablet);
        let store = Arc::new(emu_vmm::display::FrameStore::default());
        info.display = Some(store.clone());
        let devices = attach_inputs(&info, false);
        // The 720x1280 panel, its top-left pixel red.
        let mut px = vec![0u8; 720 * 1280 * 4];
        px[..4].copy_from_slice(&[255, 0, 0, 255]);
        store.present(720, 1280, 720 * 4, sim_core::pixels::PixelFormat::RGBA_VIRTIO, &px);
        assert_eq!(run(&info, "rotation"), "rotation: 0\r\nOK");
        store.set_rotation(sim_core::rotation::Rotation::Deg90);
        assert_eq!(run(&info, "rotation"), "rotation: 90\r\nOK");
        // An image whose rotation emu doesn't read says so rather than a turn it never saw.
        let unread = ConsoleInfo { rotation_unknown: true, ..console(FormFactor::Tablet) };
        assert_eq!(run(&unread, "rotation"), "rotation: unknown\r\nOK");
        let path = std::env::temp_dir().join(format!("emu-console-turned-{}.png", std::process::id()));
        let reply = run(&info, &format!("screenshot {}", path.display()));
        let png = std::fs::read(&path).expect("written");
        let _ = std::fs::remove_file(&path);
        assert_eq!(reply, format!("{} (1280x720, frame 2)\r\nOK", path.display()));
        assert_eq!(sim_core::frame::png_size(&png), Some((1280, 720)));
        let corner = store.latest_upright(|c| c.rgba[1279 * 4..1280 * 4].to_vec());
        assert_eq!(corner.as_deref(), Some(&[255, 0, 0, 255][..]), "the panel's corner at the screen's top right");
        // A touch and a pen at the screen's top right press the panel's top left.
        let axes = |batch: &[InputEvent]| {
            let at = |code| batch.iter().find(|e| e.typ == EV_ABS && e.code == code).map(|e| e.value);
            (at(ABS_MT_POSITION_X), at(ABS_MT_POSITION_Y))
        };
        assert_eq!(run(&info, "touch 1279 0 down"), "OK");
        assert_eq!(run(&info, "pen 1279 0 down"), "OK");
        assert_eq!(run(&info, "pen 1279 720 down"), "KO: (1279, 720) is off the 1280x720 screen");
        let batches = devices.touch.expect("touch").take_queued();
        assert_eq!(batches.len(), 2);
        for b in &batches {
            assert_eq!(axes(b), (Some(0), Some(0)), "{b:?}");
        }
    }

    #[test]
    fn foldables_advertise_and_take_the_hinge_angle() {
        let fold = console(FormFactor::Foldable);
        assert!(run(&fold, "sensor status").contains("hinge-angle0: enabled."));
        assert_eq!(run(&fold, "sensor set hinge-angle0 90"), "OK");
        assert_eq!(run(&fold, "sensor get hinge-angle0"), "hinge-angle0 = 90\r\nOK");
        assert_eq!(run(&fold, "sensor set hinge-angle0 90:1"), "KO: bad sensor 'hinge-angle0' or value count");
        assert!(!run(&console(FormFactor::Phone), "sensor status").contains("hinge"), "phones have no hinge");
        assert_eq!(run(&fold, "sensor set hinge-angle0 270"), "KO: hinge angle 270 is outside 0-180");
    }

    #[test]
    fn foldables_fold_unfold_and_change_posture_as_stock_does() {
        let fold = console(FormFactor::Foldable);
        assert_eq!(run(&fold, "fold"), "OK");
        assert_eq!(run(&fold, "sensor get hinge-angle0"), "hinge-angle0 = 0\r\nOK");
        assert!(run(&fold, "fold status").contains("folded: true\r\nvisible: 949x1840+629+0"));
        assert_eq!(run(&fold, "posture 2"), "OK");
        assert_eq!(run(&fold, "sensor get hinge-angle0"), "hinge-angle0 = 90\r\nOK");
        assert_eq!(run(&fold, "sensor set hinge-angle0 10"), "OK");
        assert!(run(&fold, "fold status").contains("posture: closed (1)\r\nfolded: true"), "the sensor folds it too");
        assert_eq!(run(&fold, "unfold"), "OK");
        assert!(run(&fold, "fold status").contains("folded: false"));
        let phone = console(FormFactor::Phone);
        for cmd in ["fold", "unfold", "posture 1", "fold status"] {
            assert_eq!(run(&phone, cmd), "KO: Device is not foldable", "{cmd}");
        }
        assert!(run(&phone, "help").contains("posture"));
    }

    #[test]
    fn watches_get_the_heart_rate_and_rotary_wheel_events() {
        let info = console(FormFactor::Watch);
        assert!(run(&info, "sensor status").contains("heart-rate: enabled."));
        let devices = attach_inputs(&info, false);
        assert_eq!(run(&info, "event send EV_REL:REL_WHEEL:-1"), "OK");
        assert_eq!(devices.rotary.take_queued(), vec![vec![InputEvent::new(EV_REL, REL_WHEEL, -1), InputEvent::syn()]]);
        assert_eq!(run(&info, "event send EV_KEY:KEY_POWER:1 EV_KEY:KEY_POWER:0"), "OK");
        let power = sim_core::goldfish::keycode("KEY_POWER").expect("power");
        assert_eq!(
            devices.keyboard.take_queued(),
            vec![vec![InputEvent::new(EV_KEY, power, 1), InputEvent::syn()], vec![InputEvent::new(EV_KEY, power, 0), InputEvent::syn()]]
        );
        assert!(devices.touch.expect("touch").take_queued().is_empty());
        assert_eq!(run(&info, "event send EV_KEY:KEY_NOPE:1"), "KO: unknown key 'KEY_NOPE'");
        assert_eq!(run(&info, "event send EV_SYN"), "KO: bad event 'EV_SYN'");
    }

    #[test]
    fn event_text_types_capitals_with_shift() {
        let info = console(FormFactor::Tv);
        let devices = attach_inputs(&info, false);
        assert_eq!(run(&info, "event text Hi"), "OK");
        let keys: Vec<(u16, i32)> =
            devices.keyboard.take_queued().concat().into_iter().filter(|e| e.typ == EV_KEY).map(|e| (e.code, e.value)).collect();
        // Shift down, H, shift up, I (Linux keycodes 42, 35, 23).
        assert_eq!(keys, [(42, 1), (35, 1), (35, 0), (42, 0), (23, 1), (23, 0)]);
    }

    #[test]
    fn pens_hover_touch_and_leave_on_the_screen() {
        use emu_vmm::virtio::input::{
            ABS_MT_POSITION_X, ABS_MT_PRESSURE, ABS_MT_SLOT, ABS_MT_TOOL_TYPE, BTN_STYLUS, BTN_TOUCH, EV_ABS, EV_KEY, PEN_SLOT,
        };
        let info = console(FormFactor::Tablet);
        let devices = attach_inputs(&info, false);
        for line in ["pen 100 200", "pen 100 200 down pressure 200 button", "pen 120 200 move", "pen 120 200 up", "pen 120 200 leave"] {
            assert_eq!(run(&info, line), "OK", "{line}");
        }
        let batches = devices.touch.expect("touch").take_queued();
        assert_eq!(batches.len(), 5);
        assert!(batches.iter().all(|b| b[0] == InputEvent::new(EV_ABS, ABS_MT_SLOT, PEN_SLOT)), "the pen's own slot");
        assert!(batches[0].contains(&InputEvent::new(EV_ABS, ABS_MT_TOOL_TYPE, 1)));
        // Pixels and the console's pressure (of 255) on stock's axes (0..=32767, 0..=1024).
        assert!(batches[0].contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 4552)), "100 of 720: {:?}", batches[0]);
        assert!(
            batches[1].contains(&InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 803))
                && batches[1].contains(&InputEvent::new(EV_KEY, BTN_STYLUS, 1))
        );
        assert!(batches[3].contains(&InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 0)), "up lifts to hovering");
        assert!(batches.iter().flatten().all(|e| e.typ != EV_KEY || e.code != BTN_TOUCH), "contact is pressure alone");
        assert_eq!(run(&info, "pen 720 10"), "KO: (720, 10) is off the 720x1280 screen");
        assert_eq!(run(&info, "pen 1 2 press"), format!("KO: unknown word 'press' ({})", sim_core::stylus::USAGE));
    }

    #[test]
    fn glasses_touchpads_take_touches_in_their_own_units() {
        let info = console(FormFactor::Vision);
        let _devices = attach_inputs(&info, true);
        assert_eq!(run(&info, "touchpad 10 10"), "KO: this device has no touchpad (hw.touchpad0)");
        let (pad, handle) = Input::new(Kind::Touch { width: 1542, height: 297, screen: false }, emu_vmm::virtio::input::TOUCHPAD_NAME);
        info.inputs.lock().expect("inputs").as_mut().expect("inputs").touchpad = Some(handle);
        for line in ["touchpad 100 150 down", "touchpad 900 150 move", "touchpad 900 150 up"] {
            assert_eq!(run(&info, line), "OK");
        }
        let batches = pad.take_queued();
        let x = |b: &Vec<InputEvent>| b.iter().find(|e| e.code == emu_vmm::virtio::input::ABS_MT_POSITION_X).map(|e| e.value);
        assert_eq!(batches.iter().map(x).collect::<Vec<_>>(), [Some(100), Some(900), None], "a swipe forward");
        assert_eq!(run(&info, "touchpad 1 2 hold"), "KO: bad phase 'hold' (down, move, up, tap)");
    }

    #[test]
    fn no_touch_screens_refuse_touches_but_take_keys() {
        let info = console(FormFactor::Tv);
        let devices = attach(&info, false, false);
        assert_eq!(run(&info, "touch 10 20"), NO_TOUCH);
        assert_eq!(run(&info, "event send EV_ABS:0:5 EV_KEY:KEY_CENTER:1"), NO_TOUCH, "nothing half-sent");
        assert!(devices.keyboard.take_queued().is_empty());
        assert_eq!(run(&info, "event send EV_KEY:KEY_CENTER:1"), "OK");
        assert_eq!(devices.keyboard.take_queued().len(), 1);
    }

    #[test]
    fn touch_lands_on_the_touch_screen() {
        let info = console(FormFactor::Tablet);
        assert_eq!(run(&info, "touch 10 20"), "KO: usage: touch <x> <y> [down|up|tap]", "no inputs before boot");
        let devices = attach_inputs(&info, false);
        assert_eq!(run(&info, "touch 10 20"), "OK");
        let batches = devices.touch.expect("touch").take_queued();
        assert_eq!(batches.len(), 2, "down then up");
        use emu_vmm::virtio::input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID, EV_ABS};
        // Device pixels on stock's 0..=32767 axes: 10 of 720 and 20 of 1280.
        assert!(batches[0].contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 456)), "{:?}", batches[0]);
        assert!(batches[0].contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, 512)), "{:?}", batches[0]);
        assert!(batches[1].contains(&InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1)));
    }

    /// Opens the guest end of a qemud pipe the way the guest's service does.
    fn open_pipe(service: &dyn PipeService, name: &str) -> (Box<dyn emu_vmm::pipe::PipeHandler>, Receiver<WorkerEvent>) {
        let (events, host) = channel();
        (service.open(name, PipeCtx { conn: 7, events }).expect("open"), host)
    }

    fn next_frame(host: &Receiver<WorkerEvent>) -> Vec<u8> {
        match host.recv_timeout(Duration::from_secs(5)) {
            Ok(WorkerEvent::Host { conn: 7, msg: HostMsg::Data(d) }) => d,
            _ => panic!("expected data for the pipe"),
        }
    }

    #[test]
    fn xr_requests_reach_the_guest_service() {
        assert_eq!(run(&console(FormFactor::Phone), "xr recenter"), "KO: not an Android XR device");
        let info = console(FormFactor::Vision);
        assert_eq!(run(&info, "xr status"), "xr_service: not connected\r\nlast reply: none\r\nOK");
        assert_eq!(run(&info, "xr recenter"), "KO: the guest's XR service isn't connected (still booting?)");
        let xr = info.xr.clone().expect("xr");
        let (_pipe, host) = open_pipe(&XrService { xr }, "qemud:xr_service");
        assert_eq!(run(&info, "xr recenter"), "OK");
        let frame = next_frame(&host);
        let request = sim_core::xr::XrCommand::Recenter.request().expect("request");
        assert_eq!(&frame[4..], &request[..], "qemud-framed EmulatorRequest");
        assert_eq!(&frame[..4], format!("{:04x}", request.len()).as_bytes());
        assert!(run(&info, "xr status").starts_with("xr_service: connected\r\n"));
        assert!(run(&info, "xr fly").starts_with("KO: "));
    }

    #[test]
    fn xr_pointer_drives_the_dual_mode_mouse() {
        let info = console(FormFactor::Vision);
        let _no_mouse = attach_inputs(&info, false);
        assert_eq!(run(&info, "xr pointer 10 10"), "KO: no dual-mode mouse");
        let devices = attach_inputs(&info, true);
        assert_eq!(run(&info, "xr pointer 360 640 click"), "OK");
        let (x, y) = (sim_core::xr::pointer_abs(360, 720), sim_core::xr::pointer_abs(640, 1280));
        let mouse = devices.mouse.expect("mouse").take_queued();
        let left = |down| InputEvent::new(EV_KEY, emu_vmm::virtio::input::BTN_LEFT, down);
        assert_eq!(mouse.len(), 2);
        assert!(mouse[0].contains(&left(1)) && mouse[1].contains(&left(0)));
        assert!(mouse[0].contains(&InputEvent::new(emu_vmm::virtio::input::EV_ABS, emu_vmm::virtio::input::ABS_X, x)));
        assert!(mouse[0].contains(&InputEvent::new(emu_vmm::virtio::input::EV_ABS, emu_vmm::virtio::input::ABS_Y, y)));
    }

    /// Answers the next vehicle HAL command the console sends, as the guest's VHAL does.
    fn answer_vhal(pipe: &Mutex<Box<dyn emu_vmm::pipe::PipeHandler>>, host: &Receiver<WorkerEvent>, reply: impl Fn(&Message) -> Message) {
        let frame = next_frame(host);
        let sent = Message::decode(&frame[4..]).expect("decode");
        let payload = reply(&sent).encode();
        let mut guest = format!("{:04x}", payload.len()).into_bytes();
        guest.extend_from_slice(&payload);
        assert!(pipe.lock().expect("pipe").guest_data(&guest, &mut Vec::new()));
    }

    #[test]
    fn car_sets_and_reads_vehicle_properties_through_the_guest_vhal() {
        assert_eq!(run(&console(FormFactor::Phone), "car get gear"), "KO: not an Automotive image: there is no vehicle HAL");
        let info = Arc::new(console(FormFactor::Auto));
        assert!(run(&info, "car set gear drive").starts_with("KO: "), "no VHAL connected yet");
        let car = info.car.clone().expect("car");
        let (pipe, host) = open_pipe(&CarService { car }, "qemud:car");
        let pipe = Mutex::new(pipe);

        let console = info.clone();
        let set = std::thread::spawn(move || run(&console, "car set gear drive"));
        answer_vhal(&pipe, &host, |sent| {
            assert!(sent.is(MsgType::SetPropertyCmd));
            assert_eq!((sent.values[0].prop, &sent.values[0].int32_values[..]), (GEAR_SELECTION, &[8][..]));
            Message { msg_type: MsgType::SetPropertyResp as i32, status: Some(0), ..Default::default() }
        });
        assert_eq!(set.join().expect("set"), "OK");

        let console = info.clone();
        let get = std::thread::spawn(move || run(&console, "car get gear"));
        answer_vhal(&pipe, &host, |sent| {
            assert!(sent.is(MsgType::GetPropertyCmd));
            let value = PropValue { prop: GEAR_SELECTION, area_id: Some(0), int32_values: vec![8], ..Default::default() };
            Message { msg_type: MsgType::GetPropertyResp as i32, status: Some(0), values: vec![value], ..Default::default() }
        });
        let reply = get.join().expect("get");
        let reading = sim_core::vhal::Reading::parse(reply.lines().next().expect("line")).expect("reading");
        assert_eq!((reading.id, reading.area_id, reading.status.as_str()), (GEAR_SELECTION, 0, "available"));
        assert!(reply.ends_with("\r\nOK"), "{reply}");
        assert!(run(&info, "car names").contains("GEAR_SELECTION 0x11400400\r\n"));
        assert!(run(&info, "car set").starts_with("KO: "));
    }

    #[test]
    fn phone_services_answer_in_stock_format() {
        let info = console(FormFactor::Phone);
        assert_eq!(run(&info, "geo fix -122.08 37.42 10"), "OK");
        assert_eq!(run(&info, "geo fix 200 0"), "KO: bad geo fix arguments (longitude -180..180, latitude -90..90)");
        assert_eq!(run(&info, "power capacity 42"), "OK");
        assert_eq!(run(&info, "power ac off"), "OK");
        let display = run(&info, "power display");
        assert!(display.contains("AC: offline\r\n") && display.contains("capacity: 42\r\n"), "{display}");
        assert_eq!(run(&info, "gsm voice roaming"), "OK");
        assert!(run(&info, "gsm status").starts_with("gsm voice state: roaming\r\n"));
        assert_eq!(run(&info, "finger touch 0"), "KO: usage: finger touch <fingerid> | finger remove");
        assert_eq!(run(&info, "clipboard set hello there"), "OK");
        assert_eq!(run(&info, "clipboard get"), "hello there\r\nOK");
        assert_eq!(run(&info, "screenshot"), "KO: no display (started with -gpu off?)");
    }

    /// sim-android's typed controls (#160) against this console: every command it formats is one
    /// this console takes, and every reply it reads back parses.
    #[test]
    fn the_api_s_hardware_controls_run_here_and_read_back() {
        use sim_android::controls::*;
        let info = console(FormFactor::Phone);
        // Each command's reply lines, without the `OK`; a `KO` fails the test.
        let all = |commands: &[String]| -> Vec<String> {
            let mut out = Vec::new();
            for c in commands {
                let reply = run(&info, c);
                assert!(!reply.starts_with("KO"), "{c}: {reply}");
                out.extend(reply.split("\r\n").filter(|l| *l != "OK").map(str::to_owned));
            }
            out
        };
        let change =
            BatteryChange { level: Some(17), health: Some(BatteryHealth::Overheat), present: Some(true), ..BatteryChange::charging(true) };
        let mut commands = change.commands().expect("battery");
        commands.push("power display".into());
        let b = Battery::parse(&all(&commands)).expect("power display parses");
        assert_eq!((b.level, b.ac, b.status, b.health, b.present), (17, true, BatteryStatus::Charging, BatteryHealth::Overheat, true));
        for status in BatteryStatus::ALL {
            let b = Battery::parse(&all(&[format!("power status {}", status.name()), "power display".into()])).expect("parses");
            assert_eq!(b.status, status);
        }
        for health in BatteryHealth::ALL {
            let b = Battery::parse(&all(&[format!("power health {}", health.name()), "power display".into()])).expect("parses");
            assert_eq!(b.health, health);
        }

        let change = CellularChange { rssi: Some(7), voice: Some(Registration::Roaming), data: Some(Registration::Searching) };
        let mut commands = change.commands().expect("cellular");
        commands.push("gsm status".into());
        let c = Cellular::parse(&all(&commands)).expect("gsm status parses");
        assert_eq!((c.voice, c.data), (Some(Registration::Roaming), Some(Registration::Searching)));
        assert_eq!(info.telephony.signal().0, 7);
        for r in Registration::ALL {
            let c = Cellular::parse(&all(&[format!("gsm data {}", r.name()), "gsm status".into()])).expect("parses");
            assert_eq!(c.data, Some(r));
        }

        // The guest's radio HAL turns the radio on at boot; a call to a radio that's off is refused.
        info.telephony.guest_input(b"AT+CFUN=1\r");
        let calls = all(&[CallAction::Incoming.command("+15551234567").expect("call"), "gsm list".into()]);
        assert_eq!(
            Call::parse_list(&calls).expect("gsm list parses"),
            [Call { direction: "inbound".into(), number: "+15551234567".into(), state: "incoming".into() }]
        );
        let calls = all(&[CallAction::Cancel.command("+15551234567").expect("cancel"), "gsm list".into()]);
        assert_eq!(Call::parse_list(&calls).expect("parses"), []);

        all(&[sms_command("MyBank", "Your code is 1234").expect("sms")]);
        all(&[sms_command("+15551234567", "hi").expect("sms")]);

        let set = sensor_set_command("acceleration", &[1.5, 9.0, -2.0]).expect("set");
        let read = all(&[set, "sensor get acceleration".into()]);
        assert_eq!(parse_sensor("acceleration", &read).expect("sensor get parses"), [1.5, 9.0, -2.0]);
        for name in emu_vmm::pipe::sensors::NAMES.iter().filter(|n| !n.starts_with("hinge")) {
            let width = sensor_values(name).expect(name);
            all(&[sensor_set_command(name, &vec![2.0; width]).expect(name)]);
            assert_eq!(parse_sensor(name, &all(&[format!("sensor get {name}")])).expect(name), vec![2.0; width], "{name}");
        }
        assert!(parse_sensor_status(&all(&["sensor status".into()])).contains(&"acceleration".to_owned()));
    }

    #[test]
    fn screenshots_come_from_the_latest_frame() {
        use emu_vmm::display::DisplaySink;
        let mut info = console(FormFactor::Phone);
        let store = Arc::new(emu_vmm::display::FrameStore::default());
        info.display = Some(store.clone());
        assert_eq!(run(&info, "screenshot"), "KO: no frame presented yet");
        // A 2x1 BGRA scanout (virtio-gpu format 1).
        store.present(2, 1, 8, 1, &[0, 0, 255, 255, 255, 0, 0, 255]);
        let path = std::env::temp_dir().join(format!("emu-console-shot-{}.png", std::process::id()));
        let reply = run(&info, &format!("screenshot {}", path.display()));
        let png = std::fs::read(&path).expect("written");
        let _ = std::fs::remove_file(&path);
        assert_eq!(reply, format!("{} (2x1, frame 1)\r\nOK", path.display()));
        assert_eq!(sim_core::frame::png_size(&png), Some((2, 1)));
        assert_eq!(run(&info, "frames"), "1\r\nOK");
        // A posted frame is read back through the renderer, once for the stream and the shot.
        let posted = Arc::new(emu_vmm::display::FrameStore::default());
        info.display = Some(posted.clone());
        posted.posted(1, 1);
        assert_eq!(run(&info, &format!("screenshot {}", path.display())), "KO: the scanout frame couldn't be read");
        posted.set_readback(Box::new(|_, _, buf| {
            buf.fill(7);
            true
        }));
        let reply = run(&info, &format!("screenshot {}", path.display()));
        let png = std::fs::read(&path).expect("written");
        let _ = std::fs::remove_file(&path);
        assert_eq!(reply, format!("{} (1x1, frame 1)\r\nOK", path.display()));
        assert_eq!(sim_core::frame::png_size(&png), Some((1, 1)));
    }
}
