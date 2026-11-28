//! Thermal and power readings, and the rules that turn them into a state.
//!
//! Linux reads sysfs under a root (`/sys`; a fixture directory in tests): thermal zones and their
//! trip points, cpufreq, the Intel throttle counters, `power_supply`, the ACPI platform profile.
//! macOS values arrive as plain fields from `sim-coresim`; [`mac_power`] and [`mac_thermal`] shape
//! them. A missing file is `None`/"unavailable", never an error.

use std::path::{Path, PathBuf};

use crate::{Power, PowerSource, Thermal, ThermalState, ThermalZone, Trip};

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn read_num(p: &Path) -> Option<i64> {
    read(p)?.parse().ok()
}

/// `dir`'s entries named `prefix` + digits, sorted.
fn numbered(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix(prefix))
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    v.sort();
    v
}

/// Degrees from the kernel's millidegrees.
fn celsius(milli: i64) -> f64 {
    milli as f64 / 1000.0
}

/// Trip temperatures outside this range (degrees) are unset or bogus and ignored.
const PLAUSIBLE_TRIP_C: std::ops::RangeInclusive<f64> = 1.0..=150.0;

/// A trip within this many degrees of `critical`/`hot` is `critical`.
pub const CRITICAL_MARGIN_C: f64 = 2.0;
/// Within this many degrees of a `passive` trip, the kernel is about to (or does) throttle: `serious`.
pub const PASSIVE_MARGIN_C: f64 = 3.0;
/// Within this many degrees of a `passive` trip: `fair`.
pub const FAIR_MARGIN_C: f64 = 10.0;
/// A CPU's `scaling_max_freq` under this fraction of `cpuinfo_max_freq` is a thermal (or other) cap: `serious`.
pub const CAP_SERIOUS_RATIO: f64 = 0.8;
/// Under this fraction: `fair`.
pub const CAP_FAIR_RATIO: f64 = 0.95;

/// The state of the host's thermal inputs; `None` when none were readable.
pub fn linux_thermal_state(zones: &[ThermalZone], cap_ratio: Option<f64>) -> Option<(ThermalState, String)> {
    if zones.is_empty() && cap_ratio.is_none() {
        return None;
    }
    let mut worst = (ThermalState::Nominal, "no thermal zone near a trip point".to_owned());
    let mut raise = |s: ThermalState, why: String| {
        if s > worst.0 {
            worst = (s, why);
        }
    };
    for z in zones {
        for t in &z.trips {
            let near = format!("{} is {:.0} C, {:.1} C from its {} trip at {:.0} C", z.name, z.temp_c, t.margin_c, t.kind, t.temp_c);
            match t.kind.as_str() {
                "critical" | "hot" if t.margin_c <= CRITICAL_MARGIN_C => raise(ThermalState::Critical, near),
                "passive" if t.margin_c <= PASSIVE_MARGIN_C => raise(ThermalState::Serious, near),
                "passive" if t.margin_c <= FAIR_MARGIN_C => raise(ThermalState::Fair, near),
                _ => {}
            }
        }
    }
    if let Some(r) = cap_ratio {
        let why = format!("the CPU frequency cap is at {:.0}% of the maximum", r * 100.0);
        if r < CAP_SERIOUS_RATIO {
            raise(ThermalState::Serious, why);
        } else if r < CAP_FAIR_RATIO {
            raise(ThermalState::Fair, why);
        }
    }
    Some(worst)
}

fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// Thermal zones, cpufreq and throttle counters under `root` (`/sys`).
pub fn thermal(root: &Path) -> Thermal {
    let mut zones = Vec::new();
    for dir in numbered(&root.join("class/thermal"), "thermal_zone") {
        let Some(temp_c) = read_num(&dir.join("temp")).map(celsius).filter(|t| *t > -50.0) else { continue };
        let name = read(&dir.join("type")).or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        let mut trips = Vec::new();
        for n in 0..32 {
            // An unreadable trip is skipped, not the end of the list: zones can have gaps.
            let Some(at) = read_num(&dir.join(format!("trip_point_{n}_temp"))).map(celsius) else { continue };
            // Unset trips read 0 (x86_pkg_temp's passive) or -274000 (iwlwifi): not thresholds.
            if !PLAUSIBLE_TRIP_C.contains(&at) {
                continue;
            }
            let kind = read(&dir.join(format!("trip_point_{n}_type"))).unwrap_or_default();
            if matches!(kind.as_str(), "passive" | "hot" | "critical") {
                trips.push(Trip { kind, temp_c: at, margin_c: at - temp_c });
            }
        }
        zones.push(ThermalZone { name, temp_c, trips });
    }
    let (mut cur, mut cap, mut throttle) = (Vec::new(), Vec::new(), None::<u64>);
    for cpu in numbered(&root.join("devices/system/cpu"), "cpu") {
        let max = read_num(&cpu.join("cpufreq/cpuinfo_max_freq")).filter(|m| *m > 0);
        if let Some(max) = max {
            if let Some(c) = read_num(&cpu.join("cpufreq/scaling_cur_freq")) {
                cur.push((c as f64 / max as f64).min(1.0));
            }
            if let Some(c) = read_num(&cpu.join("cpufreq/scaling_max_freq")) {
                cap.push((c as f64 / max as f64).min(1.0));
            }
        }
        if let Some(n) = read_num(&cpu.join("thermal_throttle/core_throttle_count")).and_then(|n| u64::try_from(n).ok()) {
            *throttle.get_or_insert(0) += n;
        }
    }
    let cap_ratio = mean(&cap);
    let state = linux_thermal_state(&zones, cap_ratio);
    Thermal {
        available: state.is_some(),
        state: state.as_ref().map(|s| s.0),
        reason: state.map(|s| s.1),
        zones,
        freq_ratio: mean(&cur),
        freq_cap_ratio: cap_ratio,
        throttle_count: throttle,
    }
}

/// macOS `NSProcessInfo.thermalState` (0 nominal ... 3 critical).
pub fn mac_thermal(raw: Option<i64>) -> Thermal {
    let state = match raw {
        Some(0) => Some(ThermalState::Nominal),
        Some(1) => Some(ThermalState::Fair),
        Some(2) => Some(ThermalState::Serious),
        Some(n) if n >= 3 => Some(ThermalState::Critical),
        _ => None,
    };
    Thermal {
        available: state.is_some(),
        state,
        reason: state.map(|s| format!("the system thermal state is {}", s.as_str())),
        zones: Vec::new(),
        freq_ratio: None,
        freq_cap_ratio: None,
        throttle_count: None,
    }
}

/// One `/sys/class/power_supply/*` entry.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Supply {
    pub kind: String,
    pub online: Option<bool>,
    pub status: Option<String>,
    pub capacity: Option<u8>,
    /// `time_to_empty_now`, seconds.
    pub seconds_to_empty: Option<u64>,
    /// `scope`: `Device` is a peripheral's battery (a mouse), not the host's.
    pub device_scope: bool,
}

/// (source, has battery, charge %, charging, minutes remaining) from the supplies. A host without
/// a battery is on AC.
pub fn summarize_supplies(s: &[Supply]) -> (PowerSource, bool, Option<u8>, Option<bool>, Option<u32>) {
    let bats: Vec<&Supply> = s.iter().filter(|x| x.kind == "Battery" && !x.device_scope).collect();
    if bats.is_empty() {
        return (PowerSource::Ac, false, None, None, None);
    }
    let ac = s.iter().any(|x| !x.device_scope && x.kind != "Battery" && x.online == Some(true));
    let discharging = bats.iter().any(|b| b.status.as_deref() == Some("Discharging"));
    let charging = bats.iter().any(|b| b.status.as_deref() == Some("Charging"));
    let any_mains = s.iter().any(|x| !x.device_scope && x.kind != "Battery");
    let source = if ac || charging {
        PowerSource::Ac
    } else if discharging || any_mains {
        PowerSource::Battery
    } else if bats.iter().any(|b| b.status.as_deref() == Some("Full")) {
        PowerSource::Ac
    } else {
        PowerSource::Unknown
    };
    let caps: Vec<f64> = bats.iter().filter_map(|b| b.capacity).map(f64::from).collect();
    let pct = mean(&caps).map(|c| c.round() as u8);
    let mins = bats.iter().filter_map(|b| b.seconds_to_empty).filter(|s| *s > 0).max().map(|s| (s / 60) as u32);
    (source, true, pct, Some(charging), if source == PowerSource::Battery { mins } else { None })
}

/// A one-line description: `AC, no battery`, `on battery at 34%`, `AC, charging at 80%`.
pub fn power_summary(source: PowerSource, has_battery: bool, pct: Option<u8>, charging: Option<bool>, low_power: Option<bool>) -> String {
    let at = pct.map(|p| format!(" at {p}%")).unwrap_or_default();
    let mut s = match (source, has_battery) {
        (PowerSource::Ac, false) => "AC, no battery".to_owned(),
        (PowerSource::Ac, true) if charging == Some(true) => format!("AC, charging{at}"),
        (PowerSource::Ac, true) => format!("AC, battery{at}"),
        (PowerSource::Battery, _) => format!("on battery{at}"),
        (PowerSource::Unknown, _) => "power source unknown".to_owned(),
    };
    if low_power == Some(true) {
        s.push_str(", low power mode");
    }
    s
}

/// Power sources, the platform profile and the cpufreq policy under `root` (`/sys`).
pub fn power(root: &Path) -> Power {
    let supplies: Vec<Supply> = std::fs::read_dir(root.join("class/power_supply"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .map(|d| Supply {
            kind: read(&d.join("type")).unwrap_or_default(),
            online: read_num(&d.join("online")).map(|n| n != 0),
            status: read(&d.join("status")),
            capacity: read_num(&d.join("capacity")).and_then(|n| u8::try_from(n.clamp(0, 100)).ok()),
            seconds_to_empty: read_num(&d.join("time_to_empty_now")).and_then(|n| u64::try_from(n).ok()),
            device_scope: read(&d.join("scope")).as_deref() == Some("Device"),
        })
        .collect();
    let (source, has_battery, charge_percent, charging, minutes_remaining) = summarize_supplies(&supplies);
    let profile = read(&root.join("firmware/acpi/platform_profile"));
    let cpu0 = root.join("devices/system/cpu/cpu0/cpufreq");
    let low_power = profile.as_deref().map(|p| p == "low-power");
    Power {
        source,
        has_battery,
        charge_percent,
        charging,
        minutes_remaining,
        low_power,
        profile,
        governor: read(&cpu0.join("scaling_governor")),
        energy_preference: read(&cpu0.join("energy_performance_preference")),
        energy_mode: None,
        summary: power_summary(source, has_battery, charge_percent, charging, low_power),
    }
}

/// macOS power from IOKit's fields and `NSProcessInfo.isLowPowerModeEnabled`. The energy mode
/// (`pmset`'s powermode) has no API that doesn't fork, so it is unavailable (`None`).
pub fn mac_power(
    providing: Option<&str>,
    has_battery: bool,
    pct: Option<u8>,
    charging: Option<bool>,
    mins: Option<u32>,
    lpm: Option<bool>,
) -> Power {
    let source = match providing {
        Some("Battery Power") => PowerSource::Battery,
        Some("AC Power" | "UPS Power") => PowerSource::Ac,
        _ if !has_battery => PowerSource::Ac,
        _ => PowerSource::Unknown,
    };
    Power {
        source,
        has_battery,
        charge_percent: pct,
        charging,
        minutes_remaining: if source == PowerSource::Battery { mins } else { None },
        low_power: lpm,
        profile: None,
        governor: None,
        energy_preference: None,
        energy_mode: None,
        summary: power_summary(source, has_battery, pct, charging, lpm),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh fixture root under the temp dir.
    fn root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("simee-host-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("mkdir");
        p
    }

    fn put(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, text).expect("write");
    }

    fn zone(root: &Path, n: u32, kind: &str, temp: i64, trips: &[(&str, i64)]) {
        let d = format!("class/thermal/thermal_zone{n}");
        put(root, &format!("{d}/type"), &format!("{kind}\n"));
        put(root, &format!("{d}/temp"), &format!("{temp}\n"));
        for (i, (k, t)) in trips.iter().enumerate() {
            put(root, &format!("{d}/trip_point_{i}_type"), &format!("{k}\n"));
            put(root, &format!("{d}/trip_point_{i}_temp"), &format!("{t}\n"));
        }
    }

    #[test]
    fn thermal_zones_trips_and_state() {
        let r = root("thermal");
        zone(&r, 0, "x86_pkg_temp", 60_000, &[("passive", 95_000), ("critical", 105_000), ("active", 50_000)]);
        zone(&r, 1, "acpitz", 40_000, &[]);
        put(&r, "devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq", "4000000\n");
        put(&r, "devices/system/cpu/cpu0/cpufreq/scaling_max_freq", "4000000\n");
        put(&r, "devices/system/cpu/cpu0/cpufreq/scaling_cur_freq", "1000000\n");
        put(&r, "devices/system/cpu/cpu1/cpufreq/cpuinfo_max_freq", "4000000\n");
        put(&r, "devices/system/cpu/cpu1/cpufreq/scaling_max_freq", "4000000\n");
        put(&r, "devices/system/cpu/cpu1/cpufreq/scaling_cur_freq", "3000000\n");
        put(&r, "devices/system/cpu/cpu0/thermal_throttle/core_throttle_count", "3\n");
        put(&r, "devices/system/cpu/cpu1/thermal_throttle/core_throttle_count", "4\n");
        let t = thermal(&r);
        assert_eq!(t.zones.len(), 2);
        assert_eq!(t.zones[0].name, "x86_pkg_temp");
        assert_eq!(t.zones[0].trips.len(), 2, "active trips are not thermal limits");
        assert_eq!((t.zones[0].trips[0].margin_c, t.zones[0].trips[1].margin_c), (35.0, 45.0));
        assert_eq!((t.freq_ratio, t.freq_cap_ratio, t.throttle_count), (Some(0.5), Some(1.0), Some(7)));
        assert_eq!(t.state, Some(ThermalState::Nominal), "idle clocks are not a cap");
        // 80 C is 15 C from passive: still nominal; 87 C is 8 C: fair; 93 C: serious; 104 C: critical.
        for (milli, want) in [
            (80_000, ThermalState::Nominal),
            (87_000, ThermalState::Fair),
            (93_000, ThermalState::Serious),
            (104_000, ThermalState::Critical),
        ] {
            zone(&r, 0, "x86_pkg_temp", milli, &[("passive", 95_000), ("critical", 105_000)]);
            assert_eq!(thermal(&r).state, Some(want), "{milli}");
        }
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn unset_trips_are_ignored_and_a_gap_does_not_end_the_list() {
        let r = root("unset");
        // x86_pkg_temp reports an unset passive trip as 0; iwlwifi's is -274000 (absolute zero).
        zone(&r, 0, "x86_pkg_temp", 50_000, &[("passive", 0), ("critical", 105_000)]);
        zone(&r, 1, "iwlwifi_1", 45_000, &[("passive", -274_000)]);
        // Trip 0 is unreadable; trip 1 is real.
        put(&r, "class/thermal/thermal_zone2/type", "acpitz\n");
        put(&r, "class/thermal/thermal_zone2/temp", "92000\n");
        put(&r, "class/thermal/thermal_zone2/trip_point_1_type", "passive\n");
        put(&r, "class/thermal/thermal_zone2/trip_point_1_temp", "95000\n");
        let t = thermal(&r);
        assert_eq!(t.zones[0].trips.len(), 1, "the 0 C passive trip is unset");
        assert!(t.zones[1].trips.is_empty(), "-274 C is not a trip");
        assert_eq!(t.zones[2].trips.len(), 1, "a gap in the numbering is skipped, not the end");
        assert_eq!(t.state, Some(ThermalState::Serious), "92 C is 3 C from acpitz's real passive trip");
        let _ = fs::remove_dir_all(r.join("class/thermal/thermal_zone2"));
        assert_eq!(thermal(&r).state, Some(ThermalState::Nominal), "unset trips alone never raise the state");
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn a_frequency_cap_is_serious() {
        let r = root("cap");
        put(&r, "devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq", "4000000\n");
        put(&r, "devices/system/cpu/cpu0/cpufreq/scaling_max_freq", "2800000\n");
        let t = thermal(&r);
        assert_eq!((t.freq_cap_ratio, t.state), (Some(0.7), Some(ThermalState::Serious)));
        assert!(t.reason.is_some_and(|r| r.contains("70%")));
        put(&r, "devices/system/cpu/cpu0/cpufreq/scaling_max_freq", "3700000\n");
        assert_eq!(thermal(&r).state, Some(ThermalState::Fair));
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn missing_sysfs_is_unavailable_not_an_error() {
        let r = root("empty");
        let t = thermal(&r);
        assert!((t.available, t.state, t.freq_ratio, t.throttle_count) == (false, None, None, None) && t.zones.is_empty());
        let p = power(&r);
        assert_eq!((p.source, p.has_battery, p.summary.as_str()), (PowerSource::Ac, false, "AC, no battery"));
        assert_eq!((p.profile, p.governor, p.low_power), (None, None, None));
        let _ = fs::remove_dir_all(&r);
    }

    fn supply(root: &Path, name: &str, kind: &str, rest: &[(&str, &str)]) {
        put(root, &format!("class/power_supply/{name}/type"), kind);
        for (k, v) in rest {
            put(root, &format!("class/power_supply/{name}/{k}"), v);
        }
    }

    #[test]
    fn power_on_battery_with_a_low_power_profile() {
        let r = root("bat");
        supply(&r, "BAT0", "Battery\n", &[("status", "Discharging\n"), ("capacity", "34\n"), ("time_to_empty_now", "5400\n")]);
        supply(&r, "AC", "Mains\n", &[("online", "0\n")]);
        supply(&r, "hidpp_battery_0", "Battery\n", &[("status", "Discharging\n"), ("capacity", "5\n"), ("scope", "Device\n")]);
        put(&r, "firmware/acpi/platform_profile", "low-power\n");
        put(&r, "devices/system/cpu/cpu0/cpufreq/scaling_governor", "powersave\n");
        put(&r, "devices/system/cpu/cpu0/cpufreq/energy_performance_preference", "balance_power\n");
        let p = power(&r);
        assert_eq!(
            (p.source, p.has_battery, p.charge_percent, p.charging, p.minutes_remaining),
            (PowerSource::Battery, true, Some(34), Some(false), Some(90))
        );
        assert_eq!(
            (p.low_power, p.profile.as_deref(), p.governor.as_deref(), p.energy_preference.as_deref()),
            (Some(true), Some("low-power"), Some("powersave"), Some("balance_power"))
        );
        assert_eq!(p.summary, "on battery at 34%, low power mode");
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn power_on_ac_charging_and_desktops() {
        let r = root("ac");
        supply(&r, "BAT0", "Battery\n", &[("status", "Charging\n"), ("capacity", "80\n")]);
        supply(&r, "AC", "Mains\n", &[("online", "1\n")]);
        put(&r, "firmware/acpi/platform_profile", "balanced\n");
        let p = power(&r);
        assert_eq!(
            (p.source, p.low_power, p.summary.as_str(), p.minutes_remaining),
            (PowerSource::Ac, Some(false), "AC, charging at 80%", None)
        );
        let _ = fs::remove_dir_all(&r);
        let r = root("desktop");
        supply(&r, "AC", "Mains\n", &[("online", "1\n")]);
        assert_eq!(power(&r).summary, "AC, no battery");
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn mac_shapes() {
        assert_eq!(mac_thermal(Some(2)).state, Some(ThermalState::Serious));
        assert_eq!(mac_thermal(Some(0)).reason.as_deref(), Some("the system thermal state is nominal"));
        assert_eq!((mac_thermal(None).available, mac_thermal(Some(-1)).state), (false, None));
        let p = mac_power(Some("Battery Power"), true, Some(34), Some(false), Some(95), Some(true));
        assert_eq!(
            (p.source, p.minutes_remaining, p.summary.as_str()),
            (PowerSource::Battery, Some(95), "on battery at 34%, low power mode")
        );
        let p = mac_power(Some("AC Power"), false, None, None, None, Some(false));
        assert_eq!((p.source, p.summary.as_str()), (PowerSource::Ac, "AC, no battery"));
        assert_eq!(mac_power(None, true, None, None, None, None).source, PowerSource::Unknown);
    }
}
