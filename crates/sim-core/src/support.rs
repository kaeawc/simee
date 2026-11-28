//! The support matrix: what each simee feature needs of the Mac it runs on (the chip, the macOS
//! release that brought the API it calls, Xcode) and whether a host meets it, with the reason when
//! it doesn't. `simee doctor` prints it, `emu` checks it at startup, and the features themselves
//! refuse with the same reason. The floors come from the macOS SDK headers' `API_AVAILABLE`
//! annotations and Apple's documentation; docs/development.md "Support matrix" cites each one.

use std::fmt;

use serde::{Serialize, Serializer};

/// Where macOS records its release (`ProductVersion`) and build: callers read it and pass the text
/// to [`Version::from_system_version`] (one file read, no process; absent off macOS).
pub const SYSTEM_VERSION_PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";

/// A dotted release number (`26.6.2`, `15.0`, `14`), ordered numerically: `14.10` is after `14.2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Version {
        Version { major, minor, patch }
    }

    /// `major[.minor[.patch]]`, each a plain decimal number; `None` for anything else (an empty
    /// string, a fourth part, a beta suffix).
    pub fn parse(text: &str) -> Option<Version> {
        let mut parts = text.trim().split('.');
        let mut next = |required: bool| match parts.next() {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse::<u32>().ok(),
            None if !required => Some(0),
            _ => None,
        };
        let v = Version { major: next(true)?, minor: next(false)?, patch: next(false)? };
        parts.next().is_none().then_some(v)
    }

    /// `ProductVersion` and `ProductBuildVersion` of a `SystemVersion.plist`.
    pub fn from_system_version(plist: &str) -> Option<(Version, Option<String>)> {
        let entries = crate::plist::parse_dict(plist)?;
        let get = |key: &str| {
            entries.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
                crate::plist::Scalar::String(s) => Some(s.clone()),
                _ => None,
            })
        };
        Some((Version::parse(&get("ProductVersion")?)?, get("ProductBuildVersion")))
    }
}

/// `15`, `14.2`, `26.6.2`: no trailing zero parts.
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major)?;
        if self.minor > 0 || self.patch > 0 {
            write!(f, ".{}", self.minor)?;
        }
        if self.patch > 0 {
            write!(f, ".{}", self.patch)?;
        }
        Ok(())
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// The Mac's CPU family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Chip {
    AppleSilicon,
    Intel,
}

impl Chip {
    pub fn as_str(self) -> &'static str {
        match self {
            Chip::AppleSilicon => "Apple silicon",
            Chip::Intel => "Intel",
        }
    }
}

/// One row of the matrix: a feature and what it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Floor {
    pub id: &'static str,
    pub name: &'static str,
    /// `ios` or `android`: whose devices it belongs to.
    pub platform: &'static str,
    /// The chips it runs on today.
    pub chips: &'static [Chip],
    /// The issue that brings it to the other chips.
    pub planned: Option<&'static str>,
    /// The oldest macOS that has the API it calls; `None` when it sets no floor of its own.
    pub macos: Option<Version>,
    /// The API (or tool) that sets the floor.
    pub needs: &'static str,
    /// What a host below the floor loses.
    pub off: &'static str,
    /// Another feature it builds on.
    pub requires: Option<&'static str>,
}

/// The oldest Xcode simee is tested with (26 and 27 are); older ones are unverified.
pub const XCODE_TESTED: Version = Version::new(26, 0, 0);

const APPLE_SILICON: &[Chip] = &[Chip::AppleSilicon];

/// Every feature with a floor, in the order `simee doctor` prints them.
pub const FLOORS: &[Floor] = &[
    Floor {
        id: "ios",
        name: "iOS simulator devices",
        platform: "ios",
        chips: APPLE_SILICON,
        planned: Some("Intel Macs: #241"),
        macos: None,
        needs: "Xcode with a simulator runtime (tested with Xcode 26 and 27; Xcode 26 needs macOS 15.6)",
        off: "no iOS, iPadOS, watchOS, tvOS or visionOS devices",
        requires: None,
    },
    Floor {
        id: "audio_capture",
        name: "iOS audio capture and mute",
        platform: "ios",
        chips: APPLE_SILICON,
        planned: Some("Intel Macs: #241"),
        macos: Some(Version::new(14, 2, 0)),
        needs: "a Core Audio process tap (AudioHardwareCreateProcessTap)",
        off: "audio_capture and audio_mute fail",
        requires: Some("ios"),
    },
    Floor {
        id: "android",
        name: "Android emulator (emu)",
        platform: "android",
        chips: APPLE_SILICON,
        planned: Some("Intel Macs: #242"),
        macos: Some(Version::new(15, 0, 0)),
        needs: "Hypervisor.framework's in-kernel GICv3 (hv_gic_create)",
        off: "no Android devices boot",
        requires: None,
    },
    Floor {
        id: "granule_4k",
        name: "4 KiB guest memory granule",
        platform: "android",
        chips: APPLE_SILICON,
        planned: None,
        macos: Some(Version::new(26, 0, 0)),
        needs: "hv_vm_config_set_ipa_granule",
        off: "guest memory is mapped in 16 KiB pages, so host-visible GPU memory the guest maps at 4 KiB offsets can't be",
        requires: Some("android"),
    },
];

/// The floor named `id`.
pub fn floor(id: &str) -> Option<&'static Floor> {
    FLOORS.iter().find(|f| f.id == id)
}

/// Whether macOS `have` meets feature `id`'s floor; the error is the reason a caller reports:
/// "iOS audio capture and mute needs macOS 14.2 (a Core Audio process tap
/// (AudioHardwareCreateProcessTap)); this Mac runs macOS 14.1". An unknown host version passes:
/// the call itself then says what's missing.
pub fn check(id: &str, have: Option<Version>) -> Result<(), String> {
    let Some(f) = floor(id) else { return Ok(()) };
    match (f.macos, have) {
        (Some(need), Some(have)) if have < need => {
            Err(format!("{} needs macOS {need} ({}); this Mac runs macOS {have}: {}", f.name, f.needs, f.off))
        }
        _ => Ok(()),
    }
}

/// What a host offers, as `simee doctor` reads it. `None` is "couldn't tell".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Host {
    pub chip: Option<Chip>,
    pub macos: Option<Version>,
    /// The selected Xcode's version; `None` with no Xcode (or only the Command Line Tools).
    pub xcode: Option<Version>,
    /// `kern.hv_support`: the CPU and kernel can run Hypervisor.framework guests.
    pub hv_support: Option<bool>,
    /// Whether the `emu` binary is signed with `com.apple.security.hypervisor`; `None` when
    /// there is no `emu` to look at.
    pub emu_entitled: Option<bool>,
}

/// One feature on this host: on or off, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub id: &'static str,
    pub name: &'static str,
    pub platform: &'static str,
    pub on: bool,
    pub reason: String,
}

/// Every feature in [`FLOORS`] on `host`. A feature is off when the chip isn't one it runs on, the
/// macOS release is below its floor, what it builds on is off, or (iOS) there is no Xcode, or
/// (Android) the hypervisor or emu's entitlement is missing.
pub fn evaluate(host: &Host) -> Vec<Status> {
    let mut out: Vec<Status> = Vec::new();
    for f in FLOORS {
        let reason = off_reason(f, host, &out);
        let on = reason.is_none();
        let reason = reason.unwrap_or_else(|| on_reason(f, host));
        out.push(Status { id: f.id, name: f.name, platform: f.platform, on, reason });
    }
    out
}

fn off_reason(f: &Floor, host: &Host, so_far: &[Status]) -> Option<String> {
    if let Some(dep) = f.requires
        && let Some(d) = so_far.iter().find(|s| s.id == dep && !s.on)
    {
        return Some(format!("needs {}, which is off", d.name));
    }
    match host.chip {
        Some(chip) if !f.chips.contains(&chip) => {
            let planned = f.planned.map_or_else(String::new, |p| format!(" (planned: {p})"));
            return Some(format!("not on {} Macs yet{planned}", chip.as_str()));
        }
        None if host.macos.is_none() => return Some("needs macOS".to_owned()),
        _ => {}
    }
    if let Err(e) = check(f.id, host.macos) {
        // `check`'s text without the feature's name, which the status already carries.
        return Some(e.strip_prefix(f.name).map_or(e.clone(), |r| r.trim_start().to_owned()));
    }
    match f.id {
        "ios" if host.xcode.is_none() => {
            Some("no Xcode is selected (the Command Line Tools have no simulator): `xcode-select -s` or DEVELOPER_DIR".to_owned())
        }
        "android" if host.hv_support == Some(false) => {
            Some("kern.hv_support is 0: this Mac can't run Hypervisor.framework guests".to_owned())
        }
        "android" if host.emu_entitled == Some(false) => Some(
            "emu isn't signed with com.apple.security.hypervisor: `codesign -s - -f --entitlements scripts/hypervisor.entitlements <emu>`"
                .to_owned(),
        ),
        "android" if host.emu_entitled.is_none() => Some("no emu binary found (`cargo build --release -p simee-emu`)".to_owned()),
        _ => None,
    }
}

fn on_reason(f: &Floor, host: &Host) -> String {
    let os = |need: Version| host.macos.map_or_else(|| format!("macOS {need}+"), |have| format!("macOS {have} >= {need}"));
    match (f.id, f.macos) {
        ("ios", _) => match host.xcode {
            Some(x) if x < XCODE_TESTED => format!("Xcode {x} (unverified: simee is tested with Xcode {XCODE_TESTED} and later)"),
            Some(x) => format!("Xcode {x}"),
            None => "Xcode".to_owned(),
        },
        (_, Some(need)) => format!("{}: {}", os(need), f.needs),
        (_, None) => f.needs.to_owned(),
    }
}

/// The Xcode app a developer directory belongs to (`/Applications/Xcode.app/Contents/Developer`
/// → `/Applications/Xcode.app`); `None` for the Command Line Tools or any other directory.
pub fn xcode_app(developer_dir: &str) -> Option<&str> {
    developer_dir.trim_end_matches('/').strip_suffix("/Contents/Developer").filter(|app| app.ends_with(".app"))
}

/// `CFBundleShortVersionString` and `ProductBuildVersion` of an Xcode's `Contents/version.plist`.
pub fn xcode_version(plist: &str) -> Option<(Version, Option<String>)> {
    let entries = crate::plist::parse_dict(plist)?;
    let get = |key: &str| {
        entries.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            crate::plist::Scalar::String(s) => Some(s.clone()),
            _ => None,
        })
    };
    Some((Version::parse(&get("CFBundleShortVersionString")?)?, get("ProductBuildVersion")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order_numerically() {
        assert_eq!(Version::parse("26.6.2"), Some(Version::new(26, 6, 2)));
        assert_eq!(Version::parse("15"), Some(Version::new(15, 0, 0)));
        assert_eq!(Version::parse(" 14.2 "), Some(Version::new(14, 2, 0)));
        for bad in ["", "15.", ".1", "14.2.1.1", "15.0b3", "a.b", "-1", "26 .1"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert!(Version::new(14, 10, 0) > Version::new(14, 2, 0), "numeric, not lexical");
        assert!(Version::new(14, 2, 0) > Version::new(14, 1, 9));
        assert!(Version::new(15, 0, 0) > Version::new(14, 99, 99));
        assert_eq!(Version::parse("15.0.0"), Version::parse("15"));
        assert_eq!(Version::new(15, 0, 0).to_string(), "15");
        assert_eq!(Version::new(14, 2, 0).to_string(), "14.2");
        assert_eq!(Version::new(26, 0, 1).to_string(), "26.0.1");
        assert_eq!(serde_json::to_value(Version::new(26, 6, 2)).expect("json"), serde_json::json!("26.6.2"));
    }

    #[test]
    fn version_plists_parse() {
        let sys = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>ProductBuildVersion</key>
	<string>25G83</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductVersion</key>
	<string>26.6.2</string>
</dict>
</plist>"#;
        assert_eq!(Version::from_system_version(sys), Some((Version::new(26, 6, 2), Some("25G83".to_owned()))));
        let xcode = sys.replace("ProductVersion", "CFBundleShortVersionString").replace("26.6.2", "27.2").replace("25G83", "18C5");
        assert_eq!(xcode_version(&xcode), Some((Version::new(27, 2, 0), Some("18C5".to_owned()))));
        assert_eq!(Version::from_system_version("bplist00"), None);
        assert_eq!(xcode_app("/Applications/Xcode-27.2.0.app/Contents/Developer/"), Some("/Applications/Xcode-27.2.0.app"));
        assert_eq!(xcode_app("/Library/Developer/CommandLineTools"), None);
    }

    /// The table as documented in docs/development.md "Support matrix": a changed floor changes
    /// both.
    #[test]
    fn the_floor_table() {
        let rows: Vec<_> = FLOORS.iter().map(|f| (f.id, f.macos.map(|v| v.to_string()))).collect();
        let want = [("ios", None), ("audio_capture", Some("14.2")), ("android", Some("15")), ("granule_4k", Some("26"))];
        assert_eq!(rows, want.map(|(id, v)| (id, v.map(str::to_owned))));
        let docs = include_str!("../../../docs/development.md");
        for f in FLOORS {
            assert!(docs.contains(&format!("`{}`", f.id)), "docs/development.md's support matrix lists `{}`", f.id);
        }
        for f in FLOORS {
            assert!(f.requires.is_none_or(|r| FLOORS.iter().position(|g| g.id == r) < FLOORS.iter().position(|g| g.id == f.id)));
        }
    }

    #[test]
    fn check_names_the_floor_and_the_host() {
        assert_eq!(check("audio_capture", Some(Version::new(14, 2, 0))), Ok(()));
        assert_eq!(check("audio_capture", None), Ok(()), "unknown: the call says what's missing");
        let e = check("audio_capture", Some(Version::new(14, 1, 2))).expect_err("below");
        assert!(e.contains("needs macOS 14.2") && e.contains("AudioHardwareCreateProcessTap") && e.contains("runs macOS 14.1.2"), "{e}");
        let e = check("android", Some(Version::new(14, 7, 0))).expect_err("below");
        assert!(e.starts_with("Android emulator (emu) needs macOS 15 (") && e.contains("hv_gic_create"), "{e}");
        assert_eq!(check("android", Some(Version::new(15, 0, 0))), Ok(()));
        assert!(check("granule_4k", Some(Version::new(15, 7, 0))).is_err());
        assert_eq!(check("no_such_feature", Some(Version::new(1, 0, 0))), Ok(()));
    }

    fn host(macos: (u32, u32, u32)) -> Host {
        Host {
            chip: Some(Chip::AppleSilicon),
            macos: Some(Version::new(macos.0, macos.1, macos.2)),
            xcode: Some(Version::new(26, 6, 0)),
            hv_support: Some(true),
            emu_entitled: Some(true),
        }
    }

    fn on(statuses: &[Status]) -> Vec<&'static str> {
        statuses.iter().filter(|s| s.on).map(|s| s.id).collect()
    }

    #[test]
    fn evaluate_turns_off_what_the_host_lacks() {
        assert_eq!(on(&evaluate(&host((26, 6, 2)))), ["ios", "audio_capture", "android", "granule_4k"]);
        let s = evaluate(&host((15, 3, 0)));
        assert_eq!(on(&s), ["ios", "audio_capture", "android"]);
        assert!(s[3].reason.starts_with("needs macOS 26 (hv_vm_config_set_ipa_granule); this Mac runs macOS 15.3"), "{}", s[3].reason);
        let s = evaluate(&host((14, 6, 0)));
        assert_eq!(on(&s), ["ios", "audio_capture"]);
        assert_eq!(s[3].reason, "needs Android emulator (emu), which is off");
        assert_eq!(on(&evaluate(&host((14, 1, 0)))), ["ios"]);

        let intel = Host { chip: Some(Chip::Intel), ..host((15, 7, 0)) };
        let s = evaluate(&intel);
        assert_eq!(on(&s), Vec::<&str>::new());
        assert_eq!(s[0].reason, "not on Intel Macs yet (planned: Intel Macs: #241)");
        assert_eq!(s[2].reason, "not on Intel Macs yet (planned: Intel Macs: #242)");

        let no_xcode = Host { xcode: None, ..host((26, 0, 0)) };
        assert_eq!(on(&evaluate(&no_xcode)), ["android", "granule_4k"]);
        let unsigned = Host { emu_entitled: Some(false), ..host((26, 0, 0)) };
        let s = evaluate(&unsigned);
        assert!(!s[2].on && s[2].reason.contains("com.apple.security.hypervisor"), "{}", s[2].reason);
        assert!(evaluate(&Host { emu_entitled: None, ..host((26, 0, 0)) })[2].reason.contains("no emu binary"));
        assert!(evaluate(&Host { hv_support: Some(false), ..host((26, 0, 0)) })[2].reason.contains("kern.hv_support"));
        let old_xcode = Host { xcode: Some(Version::new(16, 4, 0)), ..host((15, 5, 0)) };
        let s = evaluate(&old_xcode);
        assert!(s[0].on && s[0].reason.contains("unverified"), "{}", s[0].reason);
        assert_eq!(evaluate(&Host::default()).iter().filter(|s| s.on).count(), 0, "not a Mac");
    }
}
