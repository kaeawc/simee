//! Device, runtime and device-type models, labelled with the platform that runs them.

use serde::{Deserialize, Serialize};

use crate::form_factor::{FormFactor, Kind};
use crate::platform::{Platform, SubPlatform};

/// Lifecycle state of a simulated device. Values match the service's raw state numbers.
/// Serialized with the same names `simctl list -j` uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceState {
    Creating,
    Shutdown,
    Booting,
    Booted,
    #[serde(rename = "Shutting Down")]
    ShuttingDown,
    Unknown,
}

impl DeviceState {
    /// Maps the service's raw state number.
    pub fn from_raw(raw: u64) -> Self {
        match raw {
            0 => Self::Creating,
            1 => Self::Shutdown,
            2 => Self::Booting,
            3 => Self::Booted,
            4 => Self::ShuttingDown,
            _ => Self::Unknown,
        }
    }

    /// Human-readable name, matching the service's own wording.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "Creating",
            Self::Shutdown => "Shutdown",
            Self::Booting => "Booting",
            Self::Booted => "Booted",
            Self::ShuttingDown => "Shutting Down",
            Self::Unknown => "Unknown",
        }
    }
}

/// One device: an Apple simulator device or an Android virtual device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub state: DeviceState,
    pub runtime_id: String,
    pub device_type_id: String,
    pub available: bool,
    /// Which backend runs the device.
    pub platform: Platform,
    /// Phone, tablet, watch, ...; `None` when the device type isn't recognized.
    pub form_factor: Option<FormFactor>,
    /// iPadOS, watchOS, Wear OS, ...; `None` when the device type isn't recognized.
    pub sub_platform: Option<SubPlatform>,
}

impl DeviceInfo {
    /// The validated platform and form factor, when known.
    pub fn kind(&self) -> Option<Kind> {
        Some(Kind { platform: self.platform, form_factor: self.form_factor?, sub_platform: self.sub_platform? })
    }

    /// Sets the form factor and sub-platform from `kind` (the platform stays).
    pub fn with_kind(mut self, kind: Option<Kind>) -> Self {
        self.form_factor = kind.map(|k| k.form_factor);
        self.sub_platform = kind.map(|k| k.sub_platform);
        self
    }

    /// `iPadOS · Tablet`, or just the platform when the form factor is unknown.
    pub fn label(&self) -> String {
        self.kind().map_or_else(|| self.platform.label().to_owned(), |k| k.label())
    }
}

/// An installed OS runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub identifier: String,
    pub name: String,
    pub version: String,
    pub build: String,
    pub available: bool,
    pub platform: Platform,
}

/// A device model (hardware profile).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceTypeInfo {
    pub identifier: String,
    pub name: String,
    pub model: String,
    pub product_family: String,
    /// Main screen size in pixels.
    pub screen_width: f64,
    pub screen_height: f64,
    pub screen_scale: f64,
    pub platform: Platform,
    /// From the product family; `None` for families this crate doesn't know.
    pub form_factor: Option<FormFactor>,
}

/// Maps a short privacy service name (`photos`, `camera`, ...) to its TCC service identifier.
/// Identifiers that already start with `kTCCService` pass through unchanged.
pub fn privacy_service(name: &str) -> Option<String> {
    if name.starts_with("kTCCService") {
        return Some(name.to_owned());
    }
    let id = match name {
        "all" => "All",
        "calendar" => "Calendar",
        "contacts" => "AddressBook",
        "contacts-limited" => "ContactsLimited",
        "photos" => "Photos",
        "photos-add" => "PhotosAdd",
        "media-library" => "MediaLibrary",
        "microphone" => "Microphone",
        "camera" => "Camera",
        "motion" => "Motion",
        "reminders" => "Reminders",
        "siri" => "Siri",
        "speech" => "SpeechRecognition",
        "bluetooth" => "BluetoothAlways",
        "focus" => "FocusStatus",
        _ => return None,
    };
    Some(format!("kTCCService{id}"))
}

/// How a user-supplied device reference matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    One(usize),
    None,
    Ambiguous,
}

/// Resolves `query` against devices: exact UDID (case-insensitive) first, then `booted` (the only
/// booted device), then exact name. A name shared by several devices prefers the single booted one.
pub fn select(devices: &[DeviceInfo], query: &str) -> Selection {
    if let Some(i) = devices.iter().position(|d| d.udid.eq_ignore_ascii_case(query)) {
        return Selection::One(i);
    }
    let pick = |mut it: Box<dyn Iterator<Item = usize> + '_>| match (it.next(), it.next()) {
        (Some(i), None) => Selection::One(i),
        (None, _) => Selection::None,
        _ => Selection::Ambiguous,
    };
    let booted = |d: &DeviceInfo| d.state == DeviceState::Booted;
    if query.eq_ignore_ascii_case("booted") {
        return pick(Box::new(devices.iter().enumerate().filter(|(_, d)| booted(d)).map(|(i, _)| i)));
    }
    let named = |d: &DeviceInfo| d.name == query;
    match pick(Box::new(devices.iter().enumerate().filter(|(_, d)| named(d)).map(|(i, _)| i))) {
        Selection::Ambiguous => {
            pick(Box::new(devices.iter().enumerate().filter(|(_, d)| named(d) && booted(d)).map(|(i, _)| i))).or_ambiguous()
        }
        other => other,
    }
}

/// Whether `query` has a simulator UDID's shape (8-4-4-4-12 hex digits): it selects that device
/// before any name (see [`select`]), so callers can key per-device state by it without a lookup.
pub fn is_udid(query: &str) -> bool {
    let groups: Vec<&str> = query.split('-').collect();
    groups.len() == 5 && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The environment variable that names the iOS device set a call uses when it names none (#409).
pub const DEVICE_SET_ENV: &str = "SIMEE_IOS_DEVICE_SET";

/// The CoreSimulator device set a call uses (#409): the directory `param` (a call's `set`) names,
/// else the one `env` ([`DEVICE_SET_ENV`]) names, else `None`, the default set
/// (`~/Library/Developer/CoreSimulator/Devices`, the one Simulator.app shows). An empty `param`
/// is the default set even when `env` names another; an empty `env` names none. A set is an
/// absolute path (the service resolves it, not the caller's working directory), written plainly
/// (no trailing, doubled or `.` slashes, `..` applied) so that one set has one name.
pub fn device_set(param: Option<&str>, env: Option<&str>) -> Result<Option<String>, String> {
    let (path, from) = match (param, env) {
        (Some(""), _) | (None, None | Some("")) => return Ok(None),
        (Some(path), _) => (path, "set"),
        (None, Some(path)) => (path, DEVICE_SET_ENV),
    };
    if !path.starts_with('/') {
        return Err(format!("{from} must be an absolute path to a device set's directory, not `{path}`"));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => drop(parts.pop()),
            part => parts.push(part),
        }
    }
    match parts.is_empty() {
        true => Err(format!("{from} must be a device set's own directory, not `{path}`")),
        false => Ok(Some(format!("/{}", parts.join("/")))),
    }
}

impl Selection {
    fn or_ambiguous(self) -> Self {
        if self == Self::None { Self::Ambiguous } else { self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_set_prefers_the_param_then_the_environment_then_the_default() {
        // The default: no param and no environment (or an empty one) is the default set.
        assert_eq!(device_set(None, None), Ok(None));
        assert_eq!(device_set(None, Some("")), Ok(None));
        // The environment names the set when the call doesn't.
        assert_eq!(device_set(None, Some("/farm/set")), Ok(Some("/farm/set".into())));
        // The param names the set, over the environment.
        assert_eq!(device_set(Some("/call/set"), None), Ok(Some("/call/set".into())));
        assert_eq!(device_set(Some("/call/set"), Some("/farm/set")), Ok(Some("/call/set".into())));
        // An empty param asks for the default set even when the environment names another.
        assert_eq!(device_set(Some(""), Some("/farm/set")), Ok(None));
        // One set, one name: trailing slashes are dropped.
        assert_eq!(device_set(Some("/call/set/"), None), Ok(Some("/call/set".into())));
        assert_eq!(device_set(None, Some("/farm/set//")), Ok(Some("/farm/set".into())));
        // ... and so are `.`, doubled slashes and `..` (#409): each spelling is one connection.
        for spelling in ["/farm/./set", "/farm//set", "/farm/x/../set", "/farm/set/.", "/../farm/set"] {
            assert_eq!(device_set(Some(spelling), None), Ok(Some("/farm/set".into())), "{spelling}");
        }
        assert_eq!(device_set(Some("/farm/.."), None), Err("set must be a device set's own directory, not `/farm/..`".into()));
    }

    #[test]
    fn device_set_refuses_a_relative_path() {
        let err = device_set(Some("sets/a"), None).expect_err("relative param");
        assert!(err.contains("absolute") && err.contains("sets/a"), "{err}");
        let err = device_set(None, Some("sets/b")).expect_err("relative environment");
        assert!(err.contains(DEVICE_SET_ENV) && err.contains("sets/b"), "{err}");
        // A relative environment value doesn't matter when the call names a set.
        assert_eq!(device_set(Some("/call/set"), Some("sets/b")), Ok(Some("/call/set".into())));
        assert_eq!(device_set(Some("/"), None), Err("set must be a device set's own directory, not `/`".into()));
    }

    #[test]
    fn udid_shapes() {
        assert!(is_udid("6F1C3B2A-0D4E-4F5A-9B8C-7D6E5F4A3B2C"));
        assert!(is_udid("6f1c3b2a-0d4e-4f5a-9b8c-7d6e5f4a3b2c"));
        for not in [
            "booted",
            "iPhone 17 Pro",
            "",
            "6F1C3B2A-0D4E-4F5A-9B8C",
            "6F1C3B2A-0D4E-4F5A-9B8C-7D6E5F4A3B2G",
            "6F1C3B2A0D4E4F5A9B8C7D6E5F4A3B2C",
        ] {
            assert!(!is_udid(not), "{not}");
        }
    }

    fn dev(udid: &str, name: &str, state: DeviceState) -> DeviceInfo {
        DeviceInfo {
            udid: udid.into(),
            name: name.into(),
            state,
            runtime_id: "r".into(),
            device_type_id: "t".into(),
            available: true,
            platform: Platform::Ios,
            form_factor: None,
            sub_platform: None,
        }
    }

    #[test]
    fn devices_carry_platform_labels() {
        let d = dev("A", "iPad", DeviceState::Booted);
        assert_eq!((d.kind(), d.label()), (None, "iOS".to_owned()));
        let d = d.with_kind(Kind::new(Platform::Ios, FormFactor::Tablet).ok());
        assert_eq!(d.label(), "iPadOS · Tablet");
        let json = serde_json::to_value(&d).expect("json");
        assert_eq!(
            (json["platform"].as_str(), json["form_factor"].as_str(), json["sub_platform"].as_str()),
            (Some("ios"), Some("tablet"), Some("ipados"))
        );
        assert_eq!(serde_json::from_value::<DeviceInfo>(json).expect("parse"), d);
    }

    #[test]
    fn privacy_names_map() {
        assert_eq!(privacy_service("photos").as_deref(), Some("kTCCServicePhotos"));
        assert_eq!(privacy_service("contacts").as_deref(), Some("kTCCServiceAddressBook"));
        assert_eq!(privacy_service("kTCCServiceCamera").as_deref(), Some("kTCCServiceCamera"));
        assert_eq!(privacy_service("nope"), None);
    }

    #[test]
    fn raw_states_map() {
        assert_eq!(DeviceState::from_raw(3), DeviceState::Booted);
        assert_eq!(DeviceState::from_raw(1), DeviceState::Shutdown);
        assert_eq!(DeviceState::from_raw(99), DeviceState::Unknown);
    }

    #[test]
    fn selects_by_udid_name_and_booted() {
        let ds = [
            dev("AAAA", "iPhone", DeviceState::Shutdown),
            dev("BBBB", "iPhone", DeviceState::Booted),
            dev("CCCC", "iPad", DeviceState::Shutdown),
        ];
        assert_eq!(select(&ds, "aaaa"), Selection::One(0));
        assert_eq!(select(&ds, "booted"), Selection::One(1));
        assert_eq!(select(&ds, "iPad"), Selection::One(2));
        assert_eq!(select(&ds, "iPhone"), Selection::One(1));
        assert_eq!(select(&ds, "nope"), Selection::None);
    }

    #[test]
    fn ambiguous_when_nothing_disambiguates() {
        let ds = [dev("A", "x", DeviceState::Shutdown), dev("B", "x", DeviceState::Shutdown)];
        assert_eq!(select(&ds, "x"), Selection::Ambiguous);
        assert_eq!(select(&ds, "booted"), Selection::None);
        let both = [dev("A", "x", DeviceState::Booted), dev("B", "y", DeviceState::Booted)];
        assert_eq!(select(&both, "booted"), Selection::Ambiguous);
    }
}
