//! The emulator console's hardware controls as typed calls (#160): the battery (`power`), the
//! cellular radio's signal and registration (`gsm signal`, `gsm voice|data`, `gsm status`), calls
//! (`gsm call|accept|busy|hold|cancel`, `gsm list`), incoming SMS (`sms send`), the sensors
//! (`sensor set|get|status`) and a shake (the accelerometer swung and put back).
//!
//! Each command is formatted and its arguments checked here, before the console sees them, and
//! each reply the console prints (`power display`, `gsm status`, `gsm list`, `sensor get`) is
//! read back into a value. emu's console implements every command with stock's syntax.

use std::time::Duration;

use crate::{Device, console};

/// The battery's charge state, as the console's `power status` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryStatus {
    Unknown,
    Charging,
    Discharging,
    NotCharging,
    Full,
}

impl BatteryStatus {
    pub const ALL: [BatteryStatus; 5] =
        [BatteryStatus::Unknown, BatteryStatus::Charging, BatteryStatus::Discharging, BatteryStatus::NotCharging, BatteryStatus::Full];

    /// The name `power status` takes.
    pub fn name(self) -> &'static str {
        match self {
            BatteryStatus::Unknown => "unknown",
            BatteryStatus::Charging => "charging",
            BatteryStatus::Discharging => "discharging",
            BatteryStatus::NotCharging => "not-charging",
            BatteryStatus::Full => "full",
        }
    }

    /// The name `power display` prints.
    fn display(self) -> &'static str {
        match self {
            BatteryStatus::Unknown => "Unknown",
            BatteryStatus::Charging => "Charging",
            BatteryStatus::Discharging => "Discharging",
            BatteryStatus::NotCharging => "Not charging",
            BatteryStatus::Full => "Full",
        }
    }

    pub fn parse(name: &str) -> Result<BatteryStatus, String> {
        Self::ALL
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| format!("unknown battery status `{name}` ({})", Self::ALL.map(BatteryStatus::name).join(", ")))
    }
}

/// The battery's health, as the console's `power health` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryHealth {
    Unknown,
    Good,
    Overheat,
    Dead,
    Overvoltage,
    Failure,
    Cold,
}

impl BatteryHealth {
    pub const ALL: [BatteryHealth; 7] = [
        BatteryHealth::Unknown,
        BatteryHealth::Good,
        BatteryHealth::Overheat,
        BatteryHealth::Dead,
        BatteryHealth::Overvoltage,
        BatteryHealth::Failure,
        BatteryHealth::Cold,
    ];

    /// The name `power health` takes.
    pub fn name(self) -> &'static str {
        match self {
            BatteryHealth::Unknown => "unknown",
            BatteryHealth::Good => "good",
            BatteryHealth::Overheat => "overheat",
            BatteryHealth::Dead => "dead",
            BatteryHealth::Overvoltage => "overvoltage",
            BatteryHealth::Failure => "failure",
            BatteryHealth::Cold => "cold",
        }
    }

    /// The name `power display` prints.
    fn display(self) -> &'static str {
        match self {
            BatteryHealth::Failure => "Unspecified failure",
            BatteryHealth::Unknown => "Unknown",
            BatteryHealth::Good => "Good",
            BatteryHealth::Overheat => "Overheat",
            BatteryHealth::Dead => "Dead",
            BatteryHealth::Overvoltage => "Overvoltage",
            BatteryHealth::Cold => "Cold",
        }
    }

    pub fn parse(name: &str) -> Result<BatteryHealth, String> {
        Self::ALL
            .into_iter()
            .find(|h| h.name() == name)
            .ok_or_else(|| format!("unknown battery health `{name}` ({})", Self::ALL.map(BatteryHealth::name).join(", ")))
    }
}

/// The battery as `power display` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Battery {
    /// Charge, 0 to 100 percent.
    pub level: u32,
    /// Whether the charger (AC) is plugged in.
    pub ac: bool,
    pub status: BatteryStatus,
    pub health: BatteryHealth,
    pub present: bool,
}

impl Battery {
    /// Reads `power display`'s lines: `AC: online`, `status: Charging`, `health: Good`,
    /// `present: true`, `capacity: 50`.
    pub fn parse(lines: &[String]) -> Result<Battery, String> {
        let field = |key: &str| {
            lines
                .iter()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(key)).map(|(_, v)| v.trim()))
                .ok_or_else(|| format!("`power display` has no `{key}` line: {lines:?}"))
        };
        let ac = match field("AC")? {
            "online" => true,
            "offline" => false,
            other => return Err(format!("`power display` says AC is `{other}`")),
        };
        let status = field("status")?;
        let status = BatteryStatus::ALL
            .into_iter()
            .find(|s| s.display() == status)
            .ok_or_else(|| format!("`power display` says the status is `{status}`"))?;
        let health = field("health")?;
        let health = BatteryHealth::ALL
            .into_iter()
            .find(|h| h.display() == health)
            .ok_or_else(|| format!("`power display` says the health is `{health}`"))?;
        let present = field("present")? == "true";
        let level = field("capacity")?;
        let level = level.parse().map_err(|_| format!("`power display` says the capacity is `{level}`"))?;
        Ok(Battery { level, ac, status, health, present })
    }
}

/// What to change on the battery; `None` leaves that part as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatteryChange {
    pub level: Option<u32>,
    pub ac: Option<bool>,
    pub status: Option<BatteryStatus>,
    pub health: Option<BatteryHealth>,
    pub present: Option<bool>,
}

impl BatteryChange {
    /// Plugged in and charging, or unplugged and discharging: what a charger cable does.
    pub fn charging(on: bool) -> BatteryChange {
        let status = if on { BatteryStatus::Charging } else { BatteryStatus::Discharging };
        BatteryChange { ac: Some(on), status: Some(status), ..BatteryChange::default() }
    }

    /// The `power` commands, in a fixed order (AC before status, as a charger plugs in first).
    pub fn commands(&self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        if let Some(ac) = self.ac {
            out.push(format!("power ac {}", if ac { "on" } else { "off" }));
        }
        if let Some(status) = self.status {
            out.push(format!("power status {}", status.name()));
        }
        if let Some(health) = self.health {
            out.push(format!("power health {}", health.name()));
        }
        if let Some(present) = self.present {
            out.push(format!("power present {present}"));
        }
        if let Some(level) = self.level {
            if level > 100 {
                return Err(format!("battery level {level} is past 100 percent"));
            }
            out.push(format!("power capacity {level}"));
        }
        Ok(out)
    }
}

/// A cellular registration state, as `gsm voice|data` take it and `gsm status` prints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registration {
    Home,
    Roaming,
    Searching,
    Denied,
    Unregistered,
}

impl Registration {
    pub const ALL: [Registration; 5] =
        [Registration::Home, Registration::Roaming, Registration::Searching, Registration::Denied, Registration::Unregistered];

    pub fn name(self) -> &'static str {
        match self {
            Registration::Home => "home",
            Registration::Roaming => "roaming",
            Registration::Searching => "searching",
            Registration::Denied => "denied",
            Registration::Unregistered => "unregistered",
        }
    }

    /// A state by its name; stock's `on` and `off` are `home` and `unregistered`.
    pub fn parse(name: &str) -> Result<Registration, String> {
        match name {
            "on" => Ok(Registration::Home),
            "off" => Ok(Registration::Unregistered),
            _ => Self::ALL
                .into_iter()
                .find(|r| r.name() == name)
                .ok_or_else(|| format!("unknown registration `{name}` ({})", Self::ALL.map(Registration::name).join(", "))),
        }
    }
}

/// The radio's registrations as `gsm status` reports them; `None` for a state it calls unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cellular {
    pub voice: Option<Registration>,
    pub data: Option<Registration>,
}

impl Cellular {
    /// Reads `gsm voice state: home` and `gsm data state:  home`.
    pub fn parse(lines: &[String]) -> Result<Cellular, String> {
        let state = |which: &str| -> Result<Option<Registration>, String> {
            let line = lines
                .iter()
                .find_map(|l| l.trim().strip_prefix(&format!("gsm {which} state:")))
                .ok_or_else(|| format!("`gsm status` has no {which} state: {lines:?}"))?;
            Ok(Registration::parse(line.trim()).ok())
        };
        Ok(Cellular { voice: state("voice")?, data: state("data")? })
    }
}

/// What to change on the cellular radio; `None` leaves that part as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellularChange {
    /// Signal strength, TS 27.007's rssi: 0 (-113 dBm or less) to 31 (-51 dBm or more).
    pub rssi: Option<u8>,
    pub voice: Option<Registration>,
    pub data: Option<Registration>,
}

impl CellularChange {
    pub fn commands(&self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        if let Some(voice) = self.voice {
            out.push(format!("gsm voice {}", voice.name()));
        }
        if let Some(data) = self.data {
            out.push(format!("gsm data {}", data.name()));
        }
        if let Some(rssi) = self.rssi {
            if rssi > 31 {
                return Err(format!("rssi {rssi} is past 31 (-51 dBm, full signal)"));
            }
            out.push(format!("gsm signal {rssi}"));
        }
        Ok(out)
    }
}

/// What a call's far end does: `gsm <action> <number>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallAction {
    /// The number calls the device: it rings.
    Incoming,
    /// The far end answers the device's outgoing call, or takes a held call off hold.
    Accept,
    /// The far end is busy (an outgoing call).
    Busy,
    /// The far end puts the call on hold.
    Hold,
    /// The far end hangs up (or the incoming call stops ringing).
    Cancel,
}

impl CallAction {
    pub const ALL: [CallAction; 5] = [CallAction::Incoming, CallAction::Accept, CallAction::Busy, CallAction::Hold, CallAction::Cancel];

    pub fn name(self) -> &'static str {
        match self {
            CallAction::Incoming => "incoming",
            CallAction::Accept => "accept",
            CallAction::Busy => "busy",
            CallAction::Hold => "hold",
            CallAction::Cancel => "cancel",
        }
    }

    pub fn parse(name: &str) -> Result<CallAction, String> {
        Self::ALL
            .into_iter()
            .find(|a| a.name() == name)
            .ok_or_else(|| format!("unknown call action `{name}` ({})", Self::ALL.map(CallAction::name).join(", ")))
    }

    /// The console command, the number checked as the modem checks it.
    pub fn command(self, number: &str) -> Result<String, String> {
        check_number(number)?;
        let verb = match self {
            CallAction::Incoming => "call",
            other => other.name(),
        };
        Ok(format!("gsm {verb} {number}"))
    }
}

/// A phone number as the modem takes one: digits, `*` and `#`, after an optional `+`, at most 40.
pub fn check_number(number: &str) -> Result<(), String> {
    let digits = number.strip_prefix('+').unwrap_or(number);
    if digits.is_empty() || digits.len() > 40 || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'*' || b == b'#') {
        return Err(format!("bad phone number `{number}`: digits, * and # after an optional +, at most 40"));
    }
    Ok(())
}

/// One call as `gsm list` prints it: `inbound from 5551234   : incoming`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// `inbound` or `outbound`.
    pub direction: String,
    pub number: String,
    /// `incoming`, `waiting`, `active`, `held`, `ringing` (the far end of an outgoing call) or
    /// `unknown` (dialing).
    pub state: String,
}

impl Call {
    pub fn parse_list(lines: &[String]) -> Result<Vec<Call>, String> {
        lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let bad = || format!("`gsm list` line `{l}` isn't `<inbound from|outbound to> <number> : <state>`");
                let (left, state) = l.rsplit_once(':').ok_or_else(bad)?;
                let mut words = left.split_whitespace();
                let direction = match (words.next(), words.next()) {
                    (Some("inbound"), Some("from")) => "inbound",
                    (Some("outbound"), Some("to")) => "outbound",
                    _ => return Err(bad()),
                };
                let number = words.next().ok_or_else(bad)?;
                Ok(Call { direction: direction.to_owned(), number: number.to_owned(), state: state.trim().to_owned() })
            })
            .collect()
    }
}

/// `sms send <from> <text>`. The sender is a number (as [`check_number`], at most 20) or a name
/// of at most 11 letters and digits (an alphanumeric sender, such as a bank's). The console reads
/// a line, so the text has no line breaks, and its runs of spaces arrive as one space.
pub fn sms_command(from: &str, text: &str) -> Result<String, String> {
    let numeric = check_number(from).is_ok();
    let named = !from.is_empty() && from.len() <= 11 && from.bytes().all(|b| b.is_ascii_alphanumeric());
    if !(numeric && from.len() <= 20 || named) {
        return Err(format!(
            "bad SMS sender `{from}`: a phone number of at most 20 characters, or a name of at most 11 letters and digits"
        ));
    }
    if text.trim().is_empty() {
        return Err("the SMS text is empty".into());
    }
    if text.contains(['\r', '\n']) {
        return Err("the SMS text has a line break, which the console can't carry: send each line as its own SMS".into());
    }
    Ok(format!("sms send {from} {}", text.split_whitespace().collect::<Vec<_>>().join(" ")))
}

/// The sensors the console sets, with how many values each takes (emu's and stock's console
/// spellings; `magnetic-field` is stock's name for `magnetic`).
pub fn sensor_values(name: &str) -> Option<usize> {
    let name = match name {
        "magnetic-field" => "magnetic",
        "magnetic-field-uncalibrated" => "magnetic-uncalibrated",
        other => other,
    };
    sim_core::goldfish::sensor_values(name)
}

/// `sensor set <name> <v:v:v>`, the count checked against the sensor's.
pub fn sensor_set_command(name: &str, values: &[f64]) -> Result<String, String> {
    let want = sensor_values(name).ok_or_else(|| format!("unknown sensor `{name}` ({})", sim_core::goldfish::SENSORS.join(", ")))?;
    if values.len() != want {
        return Err(format!("sensor `{name}` takes {want} value{}, not {}", if want == 1 { "" } else { "s" }, values.len()));
    }
    if let Some(v) = values.iter().find(|v| !v.is_finite()) {
        return Err(format!("sensor value {v} isn't a finite number"));
    }
    Ok(format!("sensor set {name} {}", values.iter().map(f64::to_string).collect::<Vec<_>>().join(":")))
}

/// Reads `sensor get`'s `acceleration = 0:9.80665:0`.
pub fn parse_sensor(name: &str, lines: &[String]) -> Result<Vec<f64>, String> {
    let values = lines
        .iter()
        .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == name).map(|(_, v)| v.trim()))
        .ok_or_else(|| format!("`sensor get {name}` answered {lines:?}"))?;
    values.split(':').map(|v| v.trim().parse::<f64>().map_err(|_| format!("`sensor get {name}` value `{v}` isn't a number"))).collect()
}

/// Reads `sensor status`'s `acceleration: enabled.` lines: the sensors the device has.
pub fn parse_sensor_status(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|l| l.split_once(':').filter(|(_, s)| s.trim().starts_with("enabled")).map(|(n, _)| n.trim().to_owned()))
        .collect()
}

/// How a shake moves the accelerometer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shake {
    pub duration: Duration,
    /// How far each swing pushes the acceleration along the device's x axis, m/s².
    pub intensity: f64,
}

/// Each swing lasts this long: under the 60 ms most shake detectors allow between samples, and
/// longer than the guest's sensor period at the rates they register for (`SENSOR_DELAY_GAME`, 20 ms).
pub const SHAKE_SWING: Duration = Duration::from_millis(50);

impl Shake {
    pub const DEFAULT: Shake = Shake { duration: Duration::from_millis(1000), intensity: 30.0 };

    /// The accelerations to set, one per [`SHAKE_SWING`], from `rest` (what the accelerometer
    /// reads now, put back after them): x swings `intensity` each way, at least two swings.
    pub fn plan(&self, rest: [f64; 3]) -> Result<Vec<[f64; 3]>, String> {
        if !(self.intensity.is_finite() && self.intensity > 0.0 && self.intensity <= 1000.0) {
            return Err(format!("shake intensity {} must be above 0 and at most 1000 m/s²", self.intensity));
        }
        if self.duration > Duration::from_secs(60) {
            return Err("a shake lasts at most 60 s (60000 ms)".into());
        }
        let swings = (self.duration.as_millis() / SHAKE_SWING.as_millis()).max(2) as usize;
        Ok((0..swings)
            .map(|i| {
                let push = if i % 2 == 0 { self.intensity } else { -self.intensity };
                [rest[0] + push, rest[1], rest[2]]
            })
            .collect())
    }
}

impl Device {
    /// Applies `change` to the battery, then reads it back (`power display`).
    pub fn battery(&self, change: &BatteryChange) -> Result<Battery, String> {
        let mut commands = change.commands()?;
        commands.push("power display".into());
        Battery::parse(&self.console(&commands)?)
    }

    /// Applies `change` to the cellular radio, then reads its registrations back (`gsm status`).
    pub fn cellular(&self, change: &CellularChange) -> Result<Cellular, String> {
        let mut commands = change.commands()?;
        commands.push("gsm status".into());
        Cellular::parse(&self.console(&commands)?)
    }

    /// Does `action` with the call to or from `number` (none: only lists), then lists the calls.
    pub fn phone_call(&self, action: Option<(CallAction, &str)>) -> Result<Vec<Call>, String> {
        let mut commands = Vec::new();
        if let Some((action, number)) = action {
            commands.push(action.command(number)?);
        }
        commands.push("gsm list".into());
        Call::parse_list(&self.console(&commands)?)
    }

    /// Delivers an SMS from `from` to the device.
    pub fn sms(&self, from: &str, text: &str) -> Result<(), String> {
        self.console(&[sms_command(from, text)?]).map(drop)
    }

    /// Sets sensor `name` to `values` (none: only reads) and reads it back.
    pub fn sensor(&self, name: &str, values: Option<&[f64]>) -> Result<Vec<f64>, String> {
        let mut commands = Vec::new();
        match values {
            Some(v) => commands.push(sensor_set_command(name, v)?),
            None if sensor_values(name).is_none() => {
                return Err(format!("unknown sensor `{name}` ({})", sim_core::goldfish::SENSORS.join(", ")));
            }
            None => {}
        }
        commands.push(format!("sensor get {name}"));
        parse_sensor(name, &self.console(&commands)?)
    }

    /// The sensors the device has (`sensor status`).
    pub fn sensors(&self) -> Result<Vec<String>, String> {
        Ok(parse_sensor_status(&self.console(&["sensor status".to_owned()])?))
    }

    /// Shakes the device: the accelerometer swings along x for `shake.duration`, over one console
    /// session, then reads what it read before. The rest value is put back even when a swing fails.
    pub fn shake(&self, shake: &Shake) -> Result<(), String> {
        let mut session = console::Session::open(self.running()?.console_port)?;
        let rest = parse_sensor("acceleration", &session.command("sensor get acceleration")?)?;
        let rest: [f64; 3] = rest.try_into().map_err(|v: Vec<f64>| format!("the accelerometer reads {} values, not 3", v.len()))?;
        let swings = shake.plan(rest)?;
        let set = |s: &mut console::Session, a: &[f64; 3]| s.command(&sensor_set_command("acceleration", a)?).map(drop);
        let mut result = Ok(());
        for a in &swings {
            result = set(&mut session, a);
            if result.is_err() {
                break;
            }
            std::thread::sleep(SHAKE_SWING);
        }
        let restored = set(&mut session, &rest);
        result.and(restored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn battery_changes_become_power_commands() {
        let all = BatteryChange {
            level: Some(42),
            ac: Some(false),
            status: Some(BatteryStatus::NotCharging),
            health: Some(BatteryHealth::Overheat),
            present: Some(true),
        };
        assert_eq!(
            all.commands().expect("commands"),
            ["power ac off", "power status not-charging", "power health overheat", "power present true", "power capacity 42"]
        );
        assert_eq!(BatteryChange::charging(true).commands().expect("on"), ["power ac on", "power status charging"]);
        assert_eq!(BatteryChange::charging(false).commands().expect("off"), ["power ac off", "power status discharging"]);
        assert!(BatteryChange::default().commands().expect("nothing").is_empty());
        let err = BatteryChange { level: Some(101), ..BatteryChange::default() }.commands().expect_err("101");
        assert!(err.contains("past 100"), "{err}");
    }

    #[test]
    fn battery_names_parse_and_refuse_with_the_choices() {
        for s in BatteryStatus::ALL {
            assert_eq!(BatteryStatus::parse(s.name()), Ok(s));
        }
        for h in BatteryHealth::ALL {
            assert_eq!(BatteryHealth::parse(h.name()), Ok(h));
        }
        assert!(BatteryStatus::parse("Charging").expect_err("case").contains("unknown, charging, discharging, not-charging, full"));
        assert!(BatteryHealth::parse("ok").expect_err("ok").contains("good"));
    }

    #[test]
    fn power_display_reads_back_as_emu_prints_it() {
        let shown = lines(&["AC: online", "status: Not charging", "health: Unspecified failure", "present: false", "capacity: 7"]);
        assert_eq!(
            Battery::parse(&shown),
            Ok(Battery { level: 7, ac: true, status: BatteryStatus::NotCharging, health: BatteryHealth::Failure, present: false })
        );
        // Every name emu prints reads back.
        for s in BatteryStatus::ALL {
            for h in BatteryHealth::ALL {
                let shown = lines(&["AC: offline", &format!("status: {}", s.display()), &format!("health: {}", h.display())]);
                let mut shown = shown;
                shown.extend(lines(&["present: true", "capacity: 100"]));
                let b = Battery::parse(&shown).expect("parses");
                assert_eq!((b.status, b.health, b.ac, b.level), (s, h, false, 100));
            }
        }
        assert!(Battery::parse(&lines(&["AC: online"])).expect_err("short").contains("no `status` line"));
        assert!(Battery::parse(&lines(&["AC: maybe"])).expect_err("ac").contains("AC is `maybe`"));
    }

    #[test]
    fn cellular_changes_and_status() {
        let change = CellularChange { rssi: Some(10), voice: Some(Registration::Roaming), data: Some(Registration::Unregistered) };
        assert_eq!(change.commands().expect("commands"), ["gsm voice roaming", "gsm data unregistered", "gsm signal 10"]);
        assert!(CellularChange { rssi: Some(32), ..Default::default() }.commands().expect_err("32").contains("past 31"));
        assert_eq!(Registration::parse("off"), Ok(Registration::Unregistered));
        assert_eq!(Registration::parse("on"), Ok(Registration::Home));
        assert!(Registration::parse("lost").expect_err("lost").contains("home, roaming, searching, denied, unregistered"));
        let status = lines(&["gsm voice state: home", "gsm data state:  searching"]);
        assert_eq!(Cellular::parse(&status), Ok(Cellular { voice: Some(Registration::Home), data: Some(Registration::Searching) }));
        let unknown = lines(&["gsm voice state: <unknown>", "gsm data state:  denied"]);
        assert_eq!(Cellular::parse(&unknown), Ok(Cellular { voice: None, data: Some(Registration::Denied) }));
        assert!(Cellular::parse(&lines(&["gsm voice state: home"])).expect_err("no data").contains("no data state"));
    }

    #[test]
    fn calls_are_formatted_and_numbers_checked_as_the_modem_does() {
        assert_eq!(CallAction::Incoming.command("+15551234567"), Ok("gsm call +15551234567".into()));
        assert_eq!(CallAction::Accept.command("5551234"), Ok("gsm accept 5551234".into()));
        assert_eq!(CallAction::Busy.command("*21#"), Ok("gsm busy *21#".into()));
        assert_eq!(CallAction::Hold.command("1"), Ok("gsm hold 1".into()));
        assert_eq!(CallAction::Cancel.command("1"), Ok("gsm cancel 1".into()));
        for bad in ["", "+", "555-1234", "555 1234", "abc", &"1".repeat(41)] {
            assert!(CallAction::Incoming.command(bad).expect_err(bad).contains("bad phone number"), "{bad}");
        }
        assert!(check_number(&format!("+{}", "1".repeat(40))).is_ok());
        assert!(CallAction::parse("ring").expect_err("ring").contains("incoming, accept, busy, hold, cancel"));
    }

    #[test]
    fn gsm_list_reads_back() {
        let listed = lines(&["inbound from 5551234    : incoming", "outbound to  15550000   : ringing", ""]);
        assert_eq!(
            Call::parse_list(&listed),
            Ok(vec![
                Call { direction: "inbound".into(), number: "5551234".into(), state: "incoming".into() },
                Call { direction: "outbound".into(), number: "15550000".into(), state: "ringing".into() },
            ])
        );
        assert_eq!(Call::parse_list(&[]), Ok(vec![]));
        assert!(Call::parse_list(&lines(&["sideways 1 : x"])).expect_err("bad").contains("isn't"));
    }

    #[test]
    fn sms_senders_and_texts() {
        assert_eq!(sms_command("+15551234567", "Your  code is\t1234"), Ok("sms send +15551234567 Your code is 1234".into()));
        assert_eq!(sms_command("MyBank", "hi"), Ok("sms send MyBank hi".into()));
        for bad in ["", "My Bank", "VeryLongBankName", "+", &"1".repeat(21)] {
            assert!(sms_command(bad, "hi").expect_err(bad).contains("bad SMS sender"), "{bad}");
        }
        assert!(sms_command("1", " ").expect_err("empty").contains("empty"));
        assert!(sms_command("1", "a\nb").expect_err("newline").contains("line break"));
    }

    #[test]
    fn sensors_take_their_value_counts() {
        assert_eq!(sensor_set_command("acceleration", &[0.0, 9.8, 0.5]), Ok("sensor set acceleration 0:9.8:0.5".into()));
        assert_eq!(sensor_set_command("light", &[1200.0]), Ok("sensor set light 1200".into()));
        assert_eq!(sensor_set_command("rgbc-light", &[1.0, 2.0, 3.0, 4.0]), Ok("sensor set rgbc-light 1:2:3:4".into()));
        assert_eq!(sensor_set_command("magnetic-field", &[1.0, 2.0, 3.0]), Ok("sensor set magnetic-field 1:2:3".into()));
        assert!(sensor_set_command("proximity", &[1.0, 2.0]).expect_err("count").contains("takes 1 value, not 2"));
        assert!(sensor_set_command("gyroscope", &[1.0]).expect_err("count").contains("takes 3 values, not 1"));
        assert!(sensor_set_command("smell", &[1.0]).expect_err("name").contains("unknown sensor `smell` (acceleration, gyroscope"));
        assert!(sensor_set_command("light", &[f64::NAN]).expect_err("nan").contains("finite"));
        for name in sim_core::goldfish::SENSORS {
            assert!(sensor_values(name).is_some(), "{name}");
        }
        assert_eq!(parse_sensor("acceleration", &lines(&["acceleration = 0:9.80665:0"])), Ok(vec![0.0, 9.80665, 0.0]));
        assert!(parse_sensor("light", &lines(&["acceleration = 1"])).is_err());
        let status = lines(&["acceleration: enabled.", "gyroscope: enabled.", "light: disabled."]);
        assert_eq!(parse_sensor_status(&status), ["acceleration", "gyroscope"]);
    }

    #[test]
    fn a_shake_swings_x_both_ways_around_rest() {
        let rest = [0.0, 9.8, 0.0];
        let plan = Shake::DEFAULT.plan(rest).expect("plan");
        assert_eq!(plan.len(), 20, "1 s of 50 ms swings");
        assert_eq!(plan[0], [30.0, 9.8, 0.0]);
        assert_eq!(plan[1], [-30.0, 9.8, 0.0]);
        let short = Shake { duration: Duration::from_millis(10), intensity: 5.0 }.plan([1.0, 2.0, 3.0]).expect("short");
        assert_eq!(short, [[6.0, 2.0, 3.0], [-4.0, 2.0, 3.0]], "at least one swing each way");
        for intensity in [0.0, -1.0, 1001.0, f64::INFINITY] {
            assert!(Shake { intensity, ..Shake::DEFAULT }.plan(rest).is_err(), "{intensity}");
        }
        assert!(Shake { duration: Duration::from_secs(61), ..Shake::DEFAULT }.plan(rest).expect_err("long").contains("60 s"));
    }
}
