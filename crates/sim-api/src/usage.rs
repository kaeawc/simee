//! `devices_usage`: the host sampler's per-device usage (`sim_host::usage`), each row labelled
//! with the device's name, platform and form factor from its backend's listing.

use serde_json::{Value, json};
use sim_core::device::DeviceInfo;
use sim_host::usage::{Kind, Report};

use crate::methods::Session;

/// Every running device's usage, costliest first, with simee's own and the host's totals.
pub fn devices_usage(s: &mut Session) -> Result<Value, String> {
    let report = sim_host::global().usage();
    // The simulator listing only when a simulator is running; an AVD's config only for AVDs running.
    let ios: Vec<DeviceInfo> = if report.devices.iter().any(|d| d.kind == Some(Kind::Simulator)) {
        crate::ios::call(&mut s.ios, "devices", &json!({"booted": true}))
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let android = s.android.backend();
    rows(&report, &ios, |avd| android.running_info(avd))
}

/// `report` as JSON, each device labelled from `ios` (by UDID) or `android` (by AVD name); a
/// device neither knows (another device set, a stock emulator started without an AVD) keeps its
/// id as its name and has no form factor.
pub(crate) fn rows(report: &Report, ios: &[DeviceInfo], android: impl Fn(&str) -> Option<DeviceInfo>) -> Result<Value, String> {
    let mut v = serde_json::to_value(report).map_err(|e| e.to_string())?;
    let Some(rows) = v.get_mut("devices").and_then(Value::as_array_mut) else { return Ok(v) };
    for (row, d) in rows.iter_mut().zip(&report.devices) {
        let Some(kind) = d.kind else { continue };
        let info = match kind {
            Kind::Simulator => ios.iter().find(|i| i.udid.eq_ignore_ascii_case(&d.id)).cloned(),
            Kind::Emu | Kind::Emulator => android(&d.id),
        };
        row["name"] = json!(info.as_ref().map_or(d.id.as_str(), |i| i.name.as_str()));
        row["platform"] = json!(kind.platform());
        row["form_factor"] = json!(info.as_ref().and_then(|i| i.form_factor));
        row["sub_platform"] = json!(info.as_ref().and_then(|i| i.sub_platform));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::device::DeviceState;
    use sim_core::form_factor::FormFactor;
    use sim_core::platform::Platform;
    use sim_host::usage::{Attribution, Counters, Group, Observation, Tracker};

    fn info(udid: &str, name: &str, platform: Platform, form_factor: FormFactor) -> DeviceInfo {
        let kind = sim_core::form_factor::Kind::new(platform, form_factor).ok();
        DeviceInfo {
            udid: udid.into(),
            name: name.into(),
            state: DeviceState::Booted,
            runtime_id: String::new(),
            device_type_id: String::new(),
            available: true,
            platform,
            form_factor: None,
            sub_platform: None,
        }
        .with_kind(kind)
    }

    #[test]
    fn rows_are_labelled_and_sorted_by_cost() {
        const UDID: &str = "8B529975-BB5F-4ED5-8309-0FF04764BAE6";
        let g = |kind, id: &str, pid| Group { kind, id: id.into(), root: pid, pids: vec![pid] };
        let devices = vec![g(Kind::Simulator, UDID, 1), g(Kind::Emu, "fold", 2), g(Kind::Emulator, "qemu-system-aar pid 3", 3)];
        let at = |s: u64, cpu: [u64; 3]| Observation {
            at_ns: s * 1_000_000_000,
            attribution: Attribution { devices: devices.clone(), simee: vec![] },
            counters: (1..=3).map(|pid| (pid, Counters { start_ns: 1, cpu_ns: cpu[pid as usize - 1], ..Counters::default() })).collect(),
            host_ticks: None,
        };
        let mut t = Tracker::new(4);
        t.update(at(0, [0, 0, 0]));
        t.update(at(1, [300_000_000, 900_000_000, 100_000_000]));
        let ios = [info(&UDID.to_lowercase(), "iPhone 17", Platform::Ios, FormFactor::Phone)];
        let v = rows(&t.report(), &ios, |avd| (avd == "fold").then(|| info("fold", "Pixel Fold", Platform::Android, FormFactor::Foldable)))
            .expect("rows");
        let got: Vec<(&str, &str, &str, Value, f64)> = v["devices"]
            .as_array()
            .expect("devices")
            .iter()
            .map(|d| {
                (
                    d["name"].as_str().unwrap_or(""),
                    d["kind"].as_str().unwrap_or(""),
                    d["platform"].as_str().unwrap_or(""),
                    d["form_factor"].clone(),
                    d["cpu_pct"]["1s"].as_f64().unwrap_or(-1.0),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("Pixel Fold", "emu", "android", json!("foldable"), 90.0),
                ("iPhone 17", "simulator", "ios", json!("phone"), 30.0),
                ("qemu-system-aar pid 3", "emulator", "android", Value::Null, 10.0),
            ]
        );
        assert_eq!(v["simee"]["id"], "simee");
        assert_eq!(v["host"]["cpus"], 4);
        assert!(v["host"]["devices_cpu_pct"]["1m"].as_f64().is_some_and(|c| (c - 130.0).abs() < 1e-9), "{}", v["host"]);
    }
}
