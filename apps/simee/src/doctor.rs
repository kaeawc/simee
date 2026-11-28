//! `simee doctor`: this host against the support matrix (`sim_core::support`): the chip, macOS,
//! Xcode, Hypervisor.framework, the host's health (the `host` method), and which features are on
//! or off and why. Local reads only (sysctls, version files, `xcode-select -p` when no selection
//! link exists, `codesign -d` on emu); nothing touches the network.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use sim_core::support::{self, Host, Version};

/// Where `xcode-select -s` records the selected developer directory.
const XCODE_SELECT_LINK: &str = "/var/db/xcode_select_link";

/// The developer directory and who chose it: `DEVELOPER_DIR`, else `xcode-select`'s link, else
/// `xcode-select -p` (as `sim-coresim` resolves it).
fn developer_dir() -> Option<(String, &'static str)> {
    if let Some(dir) = std::env::var("DEVELOPER_DIR").ok().filter(|d| !d.is_empty()) {
        return Some((dir, "DEVELOPER_DIR"));
    }
    if !cfg!(target_os = "macos") {
        return None;
    }
    if let Ok(target) = std::fs::read_link(XCODE_SELECT_LINK)
        && target.is_absolute()
        && target.is_dir()
    {
        return Some((target.display().to_string().trim_end_matches('/').to_owned(), "xcode-select"));
    }
    let out = Command::new("/usr/bin/xcode-select").arg("-p").output().ok()?;
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !dir.is_empty()).then_some((dir, "xcode-select -p"))
}

/// The selected Xcode: its version and build, read from `Contents/version.plist`.
fn xcode(dir: &str) -> Option<(Version, Option<String>)> {
    let app = support::xcode_app(dir)?;
    support::xcode_version(&std::fs::read_to_string(Path::new(app).join("Contents/version.plist")).ok()?)
}

/// The `emu` simee would start, when it exists (a bare `emu` is looked up on the PATH).
fn emu() -> Option<PathBuf> {
    let p = sim_android::emu_binary();
    if p.components().count() > 1 {
        return p.is_file().then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&p)).find(|c| c.is_file())
}

/// Whether `emu` carries the hypervisor entitlement (`codesign -d --entitlements`); `None` when
/// codesign can't tell (not macOS, not signed at all reads as `false`).
fn entitled(emu: &Path) -> Option<bool> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("/usr/bin/codesign").args(["-d", "--entitlements", "-", "--xml"]).arg(emu).output().ok()?;
    Some(has_hypervisor_entitlement(&String::from_utf8_lossy(&out.stdout)))
}

/// Whether an entitlements plist (as `codesign -d --entitlements - --xml` prints it) grants
/// `com.apple.security.hypervisor`.
fn has_hypervisor_entitlement(xml: &str) -> bool {
    xml.split_once("<key>com.apple.security.hypervisor</key>").is_some_and(|(_, rest)| rest.trim_start().starts_with("<true/>"))
}

/// Everything `doctor` prints, as JSON. `health` is the `host` method's report, if it answered.
pub fn report(health: Option<&Value>) -> Value {
    let m = sim_host::machine();
    let dev = developer_dir();
    let xc = dev.as_ref().and_then(|(d, _)| xcode(d));
    let emu = emu();
    let emu_entitled = emu.as_deref().and_then(entitled);
    let host = Host {
        chip: m.chip,
        macos: m.macos,
        xcode: xc.as_ref().map(|x| x.0),
        hv_support: m.hv_support,
        // An emu codesign couldn't read counts as unsigned: it can't be shown to be entitled.
        emu_entitled: emu.as_ref().map(|_| emu_entitled.unwrap_or(false)),
    };
    let features = support::evaluate(&host);
    json!({
        "chip": {"family": m.chip, "cpu": m.cpu, "model": m.model, "translated": m.translated},
        "macos": {"version": m.macos, "build": m.macos_build},
        "xcode": {
            "version": xc.as_ref().map(|x| x.0),
            "build": xc.as_ref().and_then(|x| x.1.clone()),
            "developer_dir": dev.as_ref().map(|d| &d.0),
            "selected_by": dev.as_ref().map(|d| d.1),
        },
        "hypervisor": {
            "hv_support": m.hv_support,
            "emu": emu.as_ref().map(|p| p.display().to_string()),
            "emu_entitled": emu_entitled,
        },
        "health": health.map(|h| json!({
            "thermal_state": h["thermal_state"],
            "power": h["snapshot"]["power"]["summary"],
            "verdict": h["verdict"]["verdict"],
            "reason": h["verdict"]["reason"],
        })),
        "features": features,
        "floors": support::FLOORS,
    })
}

fn s(v: &Value) -> String {
    v.as_str().map_or_else(|| if v.is_null() { "unknown".to_owned() } else { v.to_string() }, str::to_owned)
}

/// The report as text: a line per fact, then a line per feature.
pub fn text(r: &Value) -> String {
    let c = &r["chip"];
    let family = match c["family"].as_str() {
        Some("apple_silicon") => "Apple silicon",
        Some("intel") => "Intel",
        _ => "not a Mac",
    };
    let mut out = format!("chip:        {} ({}), {family}", s(&c["cpu"]), s(&c["model"]));
    if c["translated"] == json!(true) {
        out.push_str(", this simee runs under Rosetta");
    }
    out.push_str(&format!("\nmacOS:       {} ({})\n", s(&r["macos"]["version"]), s(&r["macos"]["build"])));
    let x = &r["xcode"];
    match (&x["version"], &x["developer_dir"]) {
        (Value::String(v), dir) => {
            out.push_str(&format!("Xcode:       {v} ({}) at {} ({})\n", s(&x["build"]), s(dir), s(&x["selected_by"])))
        }
        (_, Value::String(dir)) => out.push_str(&format!("Xcode:       none: {dir} ({}) is not an Xcode\n", s(&x["selected_by"]))),
        _ => out.push_str("Xcode:       none selected\n"),
    }
    let h = &r["hypervisor"];
    let hv = match h["hv_support"].as_bool() {
        Some(true) => "available (kern.hv_support 1)",
        Some(false) => "unavailable (kern.hv_support 0)",
        None => "none",
    };
    let emu = match (&h["emu"], h["emu_entitled"].as_bool()) {
        (Value::String(p), Some(true)) => format!("{p}, signed with com.apple.security.hypervisor"),
        (Value::String(p), Some(false)) => format!("{p}, NOT signed with com.apple.security.hypervisor"),
        (Value::String(p), None) => p.clone(),
        _ => "no emu found".to_owned(),
    };
    out.push_str(&format!("hypervisor:  {hv}; emu: {emu}\n"));
    if let Some(health) = r["health"].as_object() {
        out.push_str(&format!(
            "health:      thermal {}, power {}, {} ({})\n",
            s(&health["thermal_state"]),
            s(&health["power"]),
            s(&health["verdict"]),
            s(&health["reason"])
        ));
    }
    out.push_str("features:\n");
    for f in r["features"].as_array().into_iter().flatten() {
        let on = if f["on"] == json!(true) { "on " } else { "off" };
        out.push_str(&format!("  {on}  {:<14} {}: {}\n", s(&f["id"]), s(&f["name"]), s(&f["reason"])));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entitlement_must_be_true() {
        let yes = "<plist><dict><key>com.apple.security.hypervisor</key>\n\t<true/></dict></plist>";
        assert!(has_hypervisor_entitlement(yes));
        assert!(!has_hypervisor_entitlement(&yes.replace("<true/>", "<false/>")));
        assert!(!has_hypervisor_entitlement("<plist><dict><key>com.apple.security.get-task-allow</key><true/></dict></plist>"));
        assert!(!has_hypervisor_entitlement(""));
    }

    /// Smoke: the report on this host, as JSON and text.
    #[test]
    fn the_report_on_this_host() {
        let health =
            json!({"thermal_state": "nominal", "snapshot": {"power": {"summary": "AC"}}, "verdict": {"verdict": "admit", "reason": "ok"}});
        let r = report(Some(&health));
        let ids: Vec<_> = r["features"].as_array().expect("features").iter().map(|f| s(&f["id"])).collect();
        assert_eq!(ids, support::FLOORS.iter().map(|f| f.id).collect::<Vec<_>>());
        assert!(r["features"].as_array().expect("features").iter().all(|f| f["on"].is_boolean() && f["reason"].is_string()));
        assert_eq!(r["floors"][1]["macos"], json!("14.2"));
        assert_eq!(r["health"]["verdict"], json!("admit"));
        if cfg!(target_os = "macos") {
            assert!(r["macos"]["version"].is_string() && r["chip"]["cpu"].is_string(), "{r:#}");
            assert!(r["hypervisor"]["hv_support"].is_boolean());
        }
        let t = text(&r);
        assert!(t.starts_with("chip:") && t.contains("\nmacOS:") && t.contains("\nfeatures:\n  "), "{t}");
        assert!(t.contains("health:      thermal nominal, power AC, admit (ok)"), "{t}");
        println!("{t}");
    }
}
