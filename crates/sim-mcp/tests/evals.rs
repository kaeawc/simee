//! The MCP eval suite (#296): agent tasks with checkable end states, run against simee's MCP
//! server. docs/mcp-evals.md says how to run it and read its table.
//!
//! Each task in `tests/evals/*.json` is a goal, the fake backend's starting world, a reference plan
//! (the tool calls a competent agent makes, as data) and the end state to reach. The runner serves
//! each task over the stdio loop (`simee_mcp::serve`, what `simee mcp` runs) through in-memory pipes,
//! against a fake backend that models devices, apps and settings, and plays the plan as a client
//! would: `server/discover`, `tools/list`, the prompt for a prompt task, then each `tools/call`. It
//! records every message's bytes, estimates tokens, checks the end state, and writes a table to
//! `target/mcp-evals/results.md`. Each task runs under its own toolsets (`core` and what it adds)
//! and under `all`, so a longer description or schema shows its cost in both.
//!
//! The test fails when a task fails or the default (`core`) `tools/list` grows more than
//! [`GROWTH`] over `tests/evals/baseline.json`; `SIMEE_EVALS_UPDATE_BASELINE=1` rewrites it.
//! `SIMEE_EVALS_AGENT=<command>` also hands each goal to an external agent over Streamable HTTP
//! (never in CI).

use std::collections::BTreeMap;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use simee_mcp::rpc::{META_CLIENT_CAPS, META_VERSION, MODERN};
use simee_mcp::tasks::Sink;
use simee_mcp::{Backend, Toolsets};

/// How much the `core` `tools/list` may grow over the baseline before the suite fails.
const GROWTH: f64 = 0.10;
/// Rewrites the baseline instead of comparing against it.
const UPDATE_ENV: &str = "SIMEE_EVALS_UPDATE_BASELINE";
/// The live mode's agent command.
const AGENT_ENV: &str = "SIMEE_EVALS_AGENT";
/// How long one response may take.
const WAIT: Duration = Duration::from_secs(30);

fn evals_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/evals")
}

/// `target/mcp-evals`, beside the target directory's `tmp`.
fn out_dir() -> PathBuf {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    tmp.parent().unwrap_or(tmp).join("mcp-evals")
}

// ---------------------------------------------------------------------------------------------
// The fake backend: a small world of devices that the API methods change.

#[derive(Clone, Debug)]
struct Device {
    udid: String,
    name: String,
    platform: String,
    form_factor: String,
    state: String,
    apps: Vec<String>,
    foreground: Option<String>,
    appearance: String,
    orientation: String,
    posture: String,
    logs: Vec<String>,
    crashes: Vec<String>,
    /// What the last screenshot showed, as the fake PNG's text says it.
    last_shot: Option<String>,
}

impl Device {
    fn booted(&self) -> bool {
        self.state == "Booted"
    }

    /// The screen in pixels, as the orientation and hinge have it.
    fn size(&self) -> (u32, u32) {
        let (w, h) = match (self.platform.as_str(), self.form_factor.as_str()) {
            ("ios", "phone") => (1206, 2622),
            ("ios", "tablet") => (2064, 2752),
            ("android", "phone") => (1080, 2424),
            ("android", "tablet") => (1600, 2560),
            ("android", "foldable") if self.posture == "closed" => (1080, 2092),
            ("android", "foldable") => (1840, 2208),
            _ => (1000, 1000),
        };
        if self.orientation.starts_with("landscape") { (h, w) } else { (w, h) }
    }

    fn fields(&self) -> Value {
        json!({"udid": self.udid, "name": self.name, "state": self.state, "platform": self.platform, "form_factor": self.form_factor})
    }

    /// A PNG the size of the screen whose text chunk says what is on it.
    fn shot(&mut self) -> (Value, String) {
        let (w, h) = self.size();
        let text = format!(
            "simee-eval-fake device={} platform={} form_factor={} app={} appearance={} orientation={} posture={}",
            self.udid,
            self.platform,
            self.form_factor,
            self.foreground.as_deref().unwrap_or("none"),
            self.appearance,
            self.orientation,
            self.posture
        );
        self.last_shot = Some(text.clone());
        (json!({"width": w, "height": h, "png": base64(&fake_png(w, h, &text))}), text)
    }
}

/// A tap target that crashes an app.
#[derive(Clone, Debug)]
struct Crash {
    bundle_id: String,
    region: [i64; 4],
    log: String,
    report: String,
}

/// An app's icon on the home screen.
#[derive(Clone, Debug)]
struct Icon {
    bundle_id: String,
    region: [i64; 4],
}

#[derive(Debug, Default)]
struct World {
    devices: Vec<Device>,
    /// Host paths `install` takes, and the bundle id each installs.
    packages: BTreeMap<String, String>,
    crash: Option<Crash>,
    icons: Vec<Icon>,
    /// Per bundle id, per appearance, the findings `contrast_check` reports.
    contrast: Map<String, Value>,
    /// Per device, what `processes` lists.
    processes: Map<String, Value>,
    /// Every method called, in order.
    calls: Vec<String>,
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

fn region(v: &Value) -> Result<[i64; 4], String> {
    let n: Vec<i64> = v.as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
    n.try_into().map_err(|_| format!("a region is [x, y, width, height], not {v}"))
}

fn inside(r: &[i64; 4], x: i64, y: i64) -> bool {
    x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]
}

fn system_apps(platform: &str) -> Vec<String> {
    let apps: &[&str] = if platform == "ios" {
        &["com.apple.Preferences", "com.apple.mobilesafari"]
    } else {
        &["com.android.settings", "com.android.chrome"]
    };
    apps.iter().map(|a| (*a).to_owned()).collect()
}

/// Every (platform, form factor) and why one isn't supported.
const FORM_FACTORS: &[(&str, &str, Option<&str>)] = &[
    ("ios", "phone", None),
    ("ios", "tablet", None),
    ("ios", "foldable", None),
    ("ios", "watch", None),
    ("ios", "tv", None),
    ("ios", "vision", None),
    ("ios", "auto", Some("iOS has no auto form factor: CarPlay is a screen of an iPhone (`display: carplay`)")),
    ("android", "phone", None),
    ("android", "tablet", None),
    ("android", "foldable", None),
    ("android", "watch", None),
    ("android", "tv", None),
    ("android", "auto", None),
    ("android", "vision", Some("Android has no vision form factor: Android XR is `xr`")),
];

impl World {
    fn from_json(v: &Value) -> Result<World, String> {
        let mut world = World::default();
        for d in v["devices"].as_array().into_iter().flatten() {
            let field = |k: &str| text(d, k).ok_or_else(|| format!("a device needs `{k}`: {d}"));
            let platform = field("platform")?;
            let mut apps = system_apps(&platform);
            apps.extend(strings(&d["apps"]));
            world.devices.push(Device {
                udid: field("udid")?,
                name: field("name")?,
                form_factor: field("form_factor")?,
                state: field("state")?,
                foreground: text(d, "foreground"),
                appearance: text(d, "appearance").unwrap_or_else(|| "light".into()),
                orientation: text(d, "orientation").unwrap_or_else(|| "portrait".into()),
                posture: text(d, "posture").unwrap_or_else(|| "opened".into()),
                logs: strings(&d["logs"]),
                crashes: Vec::new(),
                last_shot: None,
                apps,
                platform,
            });
        }
        for (path, bundle) in v["packages"].as_object().into_iter().flatten() {
            world.packages.insert(path.clone(), bundle.as_str().unwrap_or_default().to_owned());
        }
        if let Some(c) = v.get("crash") {
            world.crash = Some(Crash {
                bundle_id: text(c, "bundle_id").ok_or("a crash needs `bundle_id`")?,
                region: region(&c["region"])?,
                log: text(c, "log").ok_or("a crash needs `log`")?,
                report: text(c, "report").ok_or("a crash needs `report`")?,
            });
        }
        for i in v["icons"].as_array().into_iter().flatten() {
            world.icons.push(Icon { bundle_id: text(i, "bundle_id").ok_or("an icon needs `bundle_id`")?, region: region(&i["region"])? });
        }
        world.contrast = v["contrast"].as_object().cloned().unwrap_or_default();
        world.processes = v["processes"].as_object().cloned().unwrap_or_default();
        Ok(world)
    }

    fn find(&self, name: &str) -> Option<usize> {
        if name == "booted" {
            return self.devices.iter().position(Device::booted);
        }
        self.devices.iter().position(|d| d.udid == name || d.name == name)
    }

    /// The device `params.device` names (`booted` when it names none).
    fn device(&mut self, params: &Value) -> Result<&mut Device, String> {
        let name = text(params, "device").unwrap_or_else(|| "booted".into());
        let i = self.find(&name).ok_or_else(|| format!("no device matches `{name}`"))?;
        Ok(&mut self.devices[i])
    }

    /// The booted device `params.device` names.
    fn running(&mut self, params: &Value) -> Result<&mut Device, String> {
        let d = self.device(params)?;
        if !d.booted() {
            return Err(format!("device `{}` is not booted (boot it first)", d.udid));
        }
        Ok(d)
    }

    fn processes_of(&mut self, params: &Value) -> Result<Vec<Value>, String> {
        let d = self.running(params)?;
        let (udid, fg) = (d.udid.clone(), d.foreground.clone());
        Ok(match self.processes.get(&udid).and_then(Value::as_array) {
            Some(list) => list.clone(),
            None => {
                let mut list = vec![
                    json!({"pid": 1, "ppid": 0, "cpu": 0.3, "rss_kb": 11000, "elapsed": "00:30:00", "name": "launchd", "path": "/sbin/launchd"}),
                ];
                if let Some(app) = fg {
                    list.push(json!({"pid": 900, "ppid": 1, "cpu": 4.0, "rss_kb": 90000, "elapsed": "00:05:00", "name": app, "path": app}));
                }
                list
            }
        })
    }

    /// The process `pid` or `bundle_id` names, for the memory tools.
    fn process(&mut self, params: &Value) -> Result<Value, String> {
        let list = self.processes_of(params)?;
        let pid = params.get("pid").and_then(Value::as_i64);
        let bundle = text(params, "bundle_id");
        list.into_iter()
            .find(|p| {
                pid.is_some_and(|n| p["pid"] == n) || bundle.as_deref().is_some_and(|b| p["path"].as_str().is_some_and(|x| x.contains(b)))
            })
            .ok_or_else(|| "no running process matches that pid or bundle_id".to_owned())
    }

    fn create(&mut self, params: &Value) -> Result<Value, String> {
        let name = text(params, "name").ok_or("`name` is required")?;
        let platform = text(params, "platform").unwrap_or_else(|| "ios".into());
        let form_factor = text(params, "form_factor").unwrap_or_else(|| "phone".into());
        match FORM_FACTORS.iter().find(|(p, f, _)| *p == platform && *f == form_factor) {
            None => return Err(format!("unknown form factor `{form_factor}` for {platform}")),
            Some((.., Some(why))) => return Err((*why).to_owned()),
            Some(_) => {}
        }
        let slug: String = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let udid = format!("created-{slug}");
        if self.find(&udid).is_some() {
            return Err(format!("a device named `{name}` exists"));
        }
        let device = Device {
            udid,
            name,
            apps: system_apps(&platform),
            platform,
            form_factor,
            state: "Shutdown".into(),
            foreground: None,
            appearance: "light".into(),
            orientation: "portrait".into(),
            posture: "opened".into(),
            logs: Vec::new(),
            crashes: Vec::new(),
            last_shot: None,
        };
        let mut out = device.fields();
        out["admission"] = json!({"verdict": "admit", "reasons": []});
        self.devices.push(device);
        Ok(out)
    }

    fn boot(&mut self, params: &Value) -> Result<Value, String> {
        let d = self.device(params)?;
        if d.booted() {
            return Ok(json!({"device": d.udid, "was_booted": true}));
        }
        d.state = "Booted".into();
        Ok(json!({"device": d.udid, "boot_s": 9.5, "was_booted": false, "admission": {"verdict": "admit", "reasons": []}}))
    }

    fn tap(&mut self, params: &Value) -> Result<Value, String> {
        let (x, y) = (params["x"].as_i64().ok_or("`x` is required")?, params["y"].as_i64().ok_or("`y` is required")?);
        let crash = self.crash.clone();
        let icons = self.icons.clone();
        let d = self.running(params)?;
        match d.foreground.clone() {
            Some(app) => {
                if let Some(c) = crash.filter(|c| c.bundle_id == app && inside(&c.region, x, y)) {
                    d.foreground = None;
                    d.logs.push(c.log);
                    d.crashes.push(c.report);
                }
            }
            None => {
                if let Some(icon) = icons.iter().find(|i| inside(&i.region, x, y) && d.apps.contains(&i.bundle_id)) {
                    d.foreground = Some(icon.bundle_id.clone());
                }
            }
        }
        Ok(Value::Null)
    }

    fn fold(&mut self, params: &Value) -> Result<Value, String> {
        let d = self.running(params)?;
        if d.form_factor != "foldable" {
            return Err(format!("`{}` is a {}, not a foldable", d.udid, d.form_factor));
        }
        let posture = match (params.get("posture").and_then(Value::as_str), params.get("folded").and_then(Value::as_bool)) {
            (Some(p), _) => p.to_owned(),
            (None, Some(true)) => "closed".into(),
            (None, Some(false)) => "opened".into(),
            (None, None) => return Err("give one of folded, angle or posture".into()),
        };
        let angle = match posture.as_str() {
            "closed" => 0,
            "half-opened" => 90,
            "opened" => 180,
            "tent" => 270,
            "flipped" => 360,
            other => return Err(format!("posture is one of closed, half-opened, opened, flipped, tent, not {other}")),
        };
        d.posture = posture;
        let (w, h) = d.size();
        Ok(json!({"angle": angle, "posture": d.posture, "folded": d.posture == "closed", "width": w, "height": h}))
    }

    fn contrast_check(&mut self, params: &Value) -> Result<Value, String> {
        let table = self.contrast.clone();
        let d = self.running(params)?;
        let app = d.foreground.clone().unwrap_or_default();
        let findings = |mode: &str| table.get(&app).map(|c| c[mode].clone()).filter(Value::is_array).unwrap_or_else(|| json!([]));
        let report = |d: &mut Device, mode: &str| {
            let f = findings(mode);
            let (shot, _) = d.shot();
            json!({"failing": f.as_array().map_or(0, Vec::len), "checked": 12, "findings": f, "png": shot["png"],
                "width": shot["width"], "height": shot["height"]})
        };
        let both = params.get("appearance").and_then(Value::as_str) == Some("both") && d.platform == "ios";
        if !both {
            let mode = d.appearance.clone();
            let mut out = report(d, &mode);
            out["scale"] = json!(3);
            out["scale_source"] = json!("device");
            return Ok(out);
        }
        let mut each = Map::new();
        let mut failing = 0;
        for mode in ["light", "dark"] {
            d.appearance = mode.into();
            let r = report(d, mode);
            failing += r["failing"].as_u64().unwrap_or(0);
            each.insert(mode.into(), r);
        }
        d.appearance = text(params, "restore").unwrap_or_else(|| "light".into());
        Ok(json!({"failing": failing, "scale": 3, "scale_source": "device", "appearances": each, "restored": d.appearance}))
    }

    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        self.calls.push(method.to_owned());
        let flag = |k: &str| params.get(k).and_then(Value::as_bool);
        match method {
            "devices" => {
                let (booted, platform, form) = (flag("booted"), text(params, "platform"), text(params, "form_factor"));
                let list: Vec<Value> = self
                    .devices
                    .iter()
                    .filter(|d| booted.is_none_or(|b| d.booted() == b))
                    .filter(|d| platform.as_ref().is_none_or(|p| &d.platform == p))
                    .filter(|d| form.as_ref().is_none_or(|f| &d.form_factor == f))
                    .map(Device::fields)
                    .collect();
                Ok(list.into())
            }
            "form_factors" => {
                let (platform, form) = (text(params, "platform"), text(params, "form_factor"));
                let list: Vec<Value> = FORM_FACTORS
                    .iter()
                    .filter(|(p, f, _)| platform.as_deref().is_none_or(|x| x == *p) && form.as_deref().is_none_or(|x| x == *f))
                    .map(|(p, f, why)| match why {
                        Some(why) => json!({"form_factor": f, "platform": p, "supported": false, "reason": why}),
                        None => json!({"form_factor": f, "platform": p, "supported": true}),
                    })
                    .collect();
                Ok(list.into())
            }
            "runtimes" => {
                let all = [
                    json!({"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-27-2", "name": "iOS 27.2", "version": "27.2",
                        "build": "25C5", "available": true, "platform": "ios"}),
                    json!({"identifier": "system-images;android-36;google_apis;arm64-v8a", "name": "Android 16 (android-36)",
                        "version": "36", "build": "36.1", "available": true, "platform": "android"}),
                ];
                let platform = text(params, "platform");
                Ok(all.into_iter().filter(|r| platform.as_deref().is_none_or(|p| r["platform"] == p)).collect::<Vec<_>>().into())
            }
            "device_types" => Ok(json!([
                {"identifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17", "name": "iPhone 17", "model": "iPhone18,3",
                 "product_family": "iPhone", "screen_width": 1206, "screen_height": 2622, "screen_scale": 3, "platform": "ios",
                 "form_factor": "phone"},
                {"identifier": "com.apple.CoreSimulator.SimDeviceType.iPad-Pro-13-inch-M5", "name": "iPad Pro 13-inch (M5)",
                 "model": "iPad17,3", "product_family": "iPad", "screen_width": 2064, "screen_height": 2752, "screen_scale": 2,
                 "platform": "ios", "form_factor": "tablet"},
            ])),
            "create" => self.create(params),
            "boot" => self.boot(params),
            "boot_many" => {
                let names = strings(&params["devices"]);
                if names.is_empty() {
                    return Err("`devices` names at least one".into());
                }
                let mut each = Vec::new();
                let mut failed = 0;
                for name in names {
                    match self.boot(&json!({"device": name})) {
                        Ok(r) => each.push(json!({"device": name, "boot_s": r["boot_s"].as_f64().unwrap_or(0.0)})),
                        Err(e) => {
                            failed += 1;
                            each.push(json!({"device": name, "error": e}));
                        }
                    }
                }
                Ok(json!({"concurrency": 2, "all_s": 19.0, "failed": failed, "unsettled": 0, "devices": each,
                    "admission": {"verdict": "admit", "reasons": []}}))
            }
            "shutdown" => {
                let d = self.device(params)?;
                d.state = "Shutdown".into();
                d.foreground = None;
                Ok(Value::Null)
            }
            "info" => Ok(self.device(params)?.fields()),
            "install" => {
                let path = text(params, "path").ok_or("`path` is required")?;
                let bundle = self.packages.get(&path).cloned().ok_or_else(|| format!("no such file: {path}"))?;
                let d = self.running(params)?;
                if !d.apps.contains(&bundle) {
                    d.apps.push(bundle);
                }
                Ok(Value::Null)
            }
            "apps" => Ok(self.running(params)?.apps.clone().into()),
            "launch" => {
                let bundle = text(params, "bundle_id").ok_or("`bundle_id` is required")?;
                let d = self.running(params)?;
                if !d.apps.contains(&bundle) {
                    return Err(format!("`{bundle}` isn't installed on {}", d.udid));
                }
                d.foreground = Some(bundle);
                Ok(json!({"pid": 4242}))
            }
            "terminate" => {
                let bundle = text(params, "bundle_id");
                let d = self.running(params)?;
                if d.foreground == bundle {
                    d.foreground = None;
                }
                Ok(Value::Null)
            }
            "screen" => {
                let (w, h) = self.running(params)?.size();
                Ok(json!({"width": w, "height": h}))
            }
            "screenshot" => Ok(self.running(params)?.shot().0),
            "tap" => self.tap(params),
            "swipe" | "type" | "key" | "touch" => self.running(params).map(|_| Value::Null),
            "button" => {
                let d = self.running(params)?;
                if params["button"] == "home" {
                    d.foreground = None;
                }
                Ok(Value::Null)
            }
            "appearance" => {
                let mode = text(params, "mode").filter(|m| m == "light" || m == "dark").ok_or("`mode` is light or dark")?;
                self.running(params)?.appearance = mode;
                Ok(Value::Null)
            }
            "rotate" => {
                const CLOCKWISE: [&str; 4] = ["portrait", "landscape-right", "portrait-upside-down", "landscape-left"];
                let d = self.running(params)?;
                d.orientation = match text(params, "orientation") {
                    Some(o) => o,
                    None => {
                        let at = CLOCKWISE.iter().position(|o| *o == d.orientation).unwrap_or(0);
                        CLOCKWISE[(at + 1) % CLOCKWISE.len()].to_owned()
                    }
                };
                Ok(json!({"orientation": d.orientation}))
            }
            "fold" => self.fold(params),
            "logs" => Ok(self.running(params)?.logs.clone().into()),
            "crash_reports" => Ok(self.running(params)?.crashes.clone().into()),
            "contrast_check" => self.contrast_check(params),
            "host" => {
                Ok(json!({"verdict": {"verdict": "admit", "reason": "memory 41% used, load 2.1 of 10", "reasons": [], "load_limit": 10},
                "thermal_state": "nominal", "load_trend": "steady", "reasons": []}))
            }
            "devices_usage" => {
                let booted: Vec<String> = self.devices.iter().filter(|d| d.booted()).map(|d| d.udid.clone()).collect();
                let mut each = Vec::new();
                for udid in booted {
                    let procs = self.processes_of(&json!({"device": udid}))?;
                    let cpu: f64 = procs.iter().filter_map(|p| p["cpu"].as_f64()).sum();
                    let rss: i64 = procs.iter().filter_map(|p| p["rss_kb"].as_i64()).sum();
                    each.push(json!({"device": udid, "cpu_pct": cpu, "rss_mb": rss / 1024}));
                }
                Ok(json!({"available": true, "samples": 10, "window_s": 10, "devices": each, "simee": {"cpu_pct": 1.2, "rss_mb": 40},
                    "host": {"cpus": 12, "memory_used_pct": 41}}))
            }
            "processes" => Ok(self.processes_of(params)?.into()),
            "mem_vmmap" => {
                let p = self.process(params)?;
                let rss = p["rss_kb"].as_i64().unwrap_or(0) * 1024;
                Ok(json!({"pid": p["pid"], "process": p["name"], "regions": [
                    {"type": "MALLOC_LARGE", "virtual": rss, "resident": rss * 9 / 10, "dirty": rss * 9 / 10, "count": 412},
                    {"type": "__TEXT", "virtual": 48_000_000, "resident": 21_000_000, "dirty": 0, "count": 96}],
                    "totals": {"resident": rss, "dirty": rss * 9 / 10}}))
            }
            "mem_heap" => {
                let p = self.process(params)?;
                let class = format!("{}ImageCache", p["name"].as_str().unwrap_or("App"));
                Ok(json!({"pid": p["pid"], "process": p["name"], "classes": [
                    {"class": class, "count": 1800, "bytes": 1_900_000_000_i64, "average": 1_055_555},
                    {"class": "NSString", "count": 40_000, "bytes": 2_100_000, "average": 52}], "nodes": 41_800, "bytes": 1_902_100_000_i64}))
            }
            "mem_leaks" => {
                let p = self.process(params)?;
                let class = format!("{}ImageCache", p["name"].as_str().unwrap_or("App"));
                Ok(json!({"pid": p["pid"], "process": p["name"], "total_leaked_bytes": 640_000_000, "leaks": [
                    {"type": class, "count": 600, "bytes": 640_000_000, "stack": ["-[HeavyImageCache store:]", "-[FeedView reload]"]}]}))
            }
            _ => Err(format!("the eval fake doesn't model `{method}`")),
        }
    }
}

/// The fake behind the server.
struct Fake(Mutex<World>);

impl Fake {
    fn world(&self) -> MutexGuard<'_, World> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Backend for Fake {
    fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
        self.world().call(method, params)
    }
}

// ---------------------------------------------------------------------------------------------
// Fake PNGs, base64, and what an image costs.

/// A PNG's signature, an IHDR of this size (as a real one lays it out, so a reader of the header
/// gets the size), and a text chunk saying what the screen shows. Not decodable as pixels.
fn fake_png(w: u32, h: u32, text: &str) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(13u32.to_be_bytes());
    png.extend(b"IHDR");
    png.extend(w.to_be_bytes());
    png.extend(h.to_be_bytes());
    png.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
    png.extend((text.len() as u32).to_be_bytes());
    png.extend(b"tEXt");
    png.extend(text.as_bytes());
    png
}

/// A PNG's size from its IHDR.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[12..16] != b"IHDR" {
        return None;
    }
    let be = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]);
    Some((be(16), be(20)))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() { char::from(B64[((n >> (18 - 6 * i)) & 63) as usize]) } else { '=' });
        }
    }
    out
}

fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let digits: Vec<u32> =
        text.bytes().filter(|b| *b != b'=').map(|b| B64.iter().position(|c| *c == b).map(|p| p as u32)).collect::<Option<_>>()?;
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, d)| n | (*d << (18 - 6 * i)));
        for i in 0..chunk.len().saturating_sub(1) {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Some(out)
}

/// An image's estimated tokens: width × height / 750, after scaling it down to fit 1568 pixels
/// on its long edge and about 1.15 megapixels, as vision models commonly resize.
fn image_tokens(w: u32, h: u32) -> u64 {
    let (w, h) = (f64::from(w), f64::from(h));
    let scale = (1568.0 / w.max(h)).min((1_150_000.0 / (w * h)).sqrt()).min(1.0);
    ((w * scale) * (h * scale) / 750.0).ceil() as u64
}

/// Text tokens, estimated as a byte count over four.
fn text_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

// ---------------------------------------------------------------------------------------------
// A stdio client: the server's own loop on in-memory pipes.

/// Lines written to the server's stdin.
struct Pipe {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    at: usize,
}

impl Read for Pipe {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.buf.len() {
            match self.rx.recv() {
                Ok(bytes) => (self.buf, self.at) = (bytes, 0),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// One request and what came back for it.
struct Exchange {
    response: Value,
    out_bytes: usize,
    /// The response and any notification before it.
    in_bytes: usize,
    /// Base64 image data in the response.
    image_bytes: usize,
    image_tokens: u64,
}

impl Exchange {
    fn tokens(&self) -> u64 {
        text_tokens(self.out_bytes + self.in_bytes - self.image_bytes) + self.image_tokens
    }

    /// The text of a resource read's contents, joined.
    fn resource_text(&self) -> String {
        let contents = self.response["result"]["contents"].as_array().cloned().unwrap_or_default();
        contents.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
    }

    /// The text blocks of a tool result, joined.
    fn text(&self) -> String {
        let content = self.response["result"]["content"].as_array().cloned().unwrap_or_default();
        content.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
    }
}

struct Client {
    stdin: Option<Sender<Vec<u8>>>,
    stdout: Receiver<String>,
    serving: Option<JoinHandle<std::io::Result<()>>>,
    next: u64,
}

impl Client {
    fn start(backend: Arc<dyn Backend>, toolsets: Toolsets) -> Client {
        let (stdin, rx) = channel::<Vec<u8>>();
        let (tx, stdout) = channel::<String>();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |line| drop(tx.lock().unwrap_or_else(|e| e.into_inner()).send(line)));
        let input = BufReader::new(Pipe { rx, buf: Vec::new(), at: 0 });
        let serving = std::thread::spawn(move || simee_mcp::serve(backend, input, sink, toolsets));
        Client { stdin: Some(stdin), stdout, serving: Some(serving), next: 1 }
    }

    /// Sends a request of the current revision and waits for its response.
    fn request(&mut self, method: &str, mut params: Value) -> Result<Exchange, String> {
        params["_meta"] = json!({META_VERSION: MODERN, META_CLIENT_CAPS: {}});
        let id = self.next;
        self.next += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        let out_bytes = line.len();
        let stdin = self.stdin.as_ref().ok_or("the client is closed")?;
        stdin.send(format!("{line}\n").into_bytes()).map_err(|_| "the server stopped reading".to_owned())?;
        let mut in_bytes = 0;
        loop {
            let line = self.stdout.recv_timeout(WAIT).map_err(|_| format!("no response to `{method}` within {WAIT:?}"))?;
            in_bytes += line.len();
            let message: Value = serde_json::from_str(&line).map_err(|e| format!("the server wrote a line that isn't JSON: {e}"))?;
            if message["id"] != id {
                continue;
            }
            let mut ex = Exchange { response: message, out_bytes, in_bytes, image_bytes: 0, image_tokens: 0 };
            for block in ex.response["result"]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "image") {
                let data = block["data"].as_str().unwrap_or_default();
                ex.image_bytes += data.len();
                ex.image_tokens +=
                    unbase64(data).as_deref().and_then(png_size).map_or(text_tokens(data.len()), |(w, h)| image_tokens(w, h));
            }
            return Ok(ex);
        }
    }

    /// Closes stdin and waits for the server to finish.
    fn finish(mut self) {
        self.stdin.take();
        if let Some(serving) = self.serving.take() {
            serving.join().expect("the server thread").expect("the server ends cleanly");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tasks.

struct Step {
    /// The tool to call, or empty for a step that reads `resource`.
    tool: String,
    /// The URI a `resources/read` step reads.
    resource: Option<String>,
    args: Value,
    expect: Value,
}

impl Step {
    /// What the step is called in a failure.
    fn label(&self) -> String {
        self.resource.clone().unwrap_or_else(|| self.tool.clone())
    }
}

struct Task {
    name: String,
    goal: String,
    /// The toolsets it needs (`core` and what it adds), as `SIMEE_MCP_TOOLSETS` spells them.
    toolsets: String,
    /// `{name, arguments}` of the prompt it follows.
    prompt: Option<Value>,
    world: Value,
    plan: Vec<Step>,
    end: Value,
}

impl Task {
    fn parse(v: &Value) -> Result<Task, String> {
        let field = |k: &str| text(v, k).ok_or_else(|| format!("a task needs `{k}`"));
        let mut plan = Vec::new();
        for s in v["plan"].as_array().ok_or("a task needs a `plan`")? {
            let resource = text(s, "resource");
            plan.push(Step {
                tool: match &resource {
                    Some(_) => String::new(),
                    None => text(s, "tool").ok_or_else(|| format!("a step needs `tool` or `resource`: {s}"))?,
                },
                resource,
                args: s.get("args").cloned().unwrap_or_else(|| json!({})),
                expect: s.get("expect").cloned().unwrap_or(Value::Null),
            });
        }
        if plan.is_empty() {
            return Err("a plan needs at least one step".into());
        }
        let end = v.get("end").filter(|e| e.as_object().is_some_and(|o| !o.is_empty())).ok_or("a task needs an `end` state")?.clone();
        Ok(Task {
            name: field("name")?,
            goal: field("goal")?,
            toolsets: field("toolsets")?,
            prompt: v.get("prompt").cloned(),
            world: v.get("world").cloned().ok_or("a task needs a `world`")?,
            plan,
            end,
        })
    }

    fn setting(&self) -> Toolsets {
        Toolsets::parse(&self.toolsets).unwrap_or_else(|e| panic!("{}: {e}", self.name))
    }
}

fn load_tasks() -> Vec<Task> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(evals_dir())
        .expect("tests/evals")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json") && p.file_stem().is_some_and(|s| s != "baseline"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(p).expect("task file"))
                .unwrap_or_else(|e| panic!("{}: not JSON: {e}", p.display()));
            let task = Task::parse(&v).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!(Some(task.name.as_str()), p.file_stem().and_then(|s| s.to_str()), "a task is named as its file");
            task
        })
        .collect()
}

/// The tools a prompt's text names: what is in backticks, before its toolsets note.
fn named_tools(prompt_text: &str) -> Vec<String> {
    let body = prompt_text.split("\n\nToolsets:").next().unwrap_or_default();
    let mut tools: Vec<String> = Vec::new();
    for word in body.split('`').skip(1).step_by(2) {
        if simee_mcp::tools::exists(word) && !tools.iter().any(|t| t == word) {
            tools.push(word.to_owned());
        }
    }
    tools
}

/// What the end state asks that `world` isn't, given what the agent saw (`seen`).
fn check_end(end: &Value, world: &World, seen: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let device = |udid: &str| world.devices.iter().find(|d| d.udid == udid);
    let each = |key: &str| end.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
    for (key, want) in [("booted", "Booted"), ("shutdown", "Shutdown")] {
        for udid in strings(&end[key]) {
            match device(&udid) {
                Some(d) if d.state == want => {}
                Some(d) => failures.push(format!("{udid} is {}, not {want}", d.state)),
                None => failures.push(format!("there is no device {udid}")),
            }
        }
    }
    let field = |d: &Device, key: &str| -> Value {
        match key {
            "foreground" => d.foreground.clone().map_or(Value::Null, Value::from),
            "appearance" => d.appearance.clone().into(),
            "orientation" => d.orientation.clone().into(),
            "posture" => d.posture.clone().into(),
            _ => Value::Null,
        }
    };
    for key in ["foreground", "appearance", "orientation", "posture"] {
        for (udid, want) in each(key) {
            match device(&udid) {
                Some(d) if field(d, key) == want => {}
                Some(d) => failures.push(format!("{udid}'s {key} is {}, not {want}", field(d, key))),
                None => failures.push(format!("there is no device {udid}")),
            }
        }
    }
    for (udid, apps) in each("installed") {
        for app in strings(&apps) {
            if !device(&udid).is_some_and(|d| d.apps.contains(&app)) {
                failures.push(format!("{app} isn't installed on {udid}"));
            }
        }
    }
    for (udid, wants) in each("screenshot_shows") {
        let shot = device(&udid).and_then(|d| d.last_shot.clone());
        for want in strings(&wants) {
            if !shot.as_deref().is_some_and(|s| s.contains(&want)) {
                failures.push(format!("{udid}'s last screenshot doesn't show {want} ({})", shot.as_deref().unwrap_or("none taken")));
            }
        }
    }
    for want in strings(&end["seen"]) {
        if !seen.contains(&want) {
            failures.push(format!("never saw {want:?}"));
        }
    }
    failures
}

/// One task under one toolset setting.
struct Run {
    task: String,
    /// `core` (the task's own toolsets) or `all`.
    setting: &'static str,
    toolsets: String,
    failures: Vec<String>,
    calls: usize,
    out_bytes: usize,
    in_bytes: usize,
    list_bytes: usize,
    message_tokens: u64,
    ms: u128,
}

impl Run {
    fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    fn list_tokens(&self) -> u64 {
        text_tokens(self.list_bytes)
    }

    /// What the run cost the agent's context: the tool list once, then every message.
    fn tokens(&self) -> u64 {
        self.list_tokens() + self.message_tokens
    }
}

/// Plays `plan` (a task's own, or another for the tests that check the checker).
fn run_plan(task: &Task, plan: &[Step], setting: &'static str, toolsets: Toolsets) -> Run {
    let started = Instant::now();
    let world = World::from_json(&task.world).unwrap_or_else(|e| panic!("{}: {e}", task.name));
    let fake = Arc::new(Fake(Mutex::new(world)));
    let label = toolsets.label();
    let mut client = Client::start(fake.clone(), toolsets);
    let mut run = Run {
        task: task.name.clone(),
        setting,
        toolsets: label,
        failures: Vec::new(),
        calls: 0,
        out_bytes: 0,
        in_bytes: 0,
        list_bytes: 0,
        message_tokens: 0,
        ms: 0,
    };
    let mut seen = String::new();
    let tally = |run: &mut Run, ex: &Exchange, is_list: bool| {
        run.out_bytes += ex.out_bytes;
        run.in_bytes += ex.in_bytes;
        if is_list {
            run.list_bytes += ex.in_bytes;
            run.message_tokens += text_tokens(ex.out_bytes);
        } else {
            run.message_tokens += ex.tokens();
        }
    };
    let mut play = || -> Result<(), String> {
        let discover = client.request("server/discover", json!({}))?;
        tally(&mut run, &discover, false);
        let list = client.request("tools/list", json!({}))?;
        tally(&mut run, &list, true);
        if let Some(prompt) = &task.prompt {
            let got = client.request("prompts/get", prompt.clone())?;
            tally(&mut run, &got, false);
            let text = got.response["result"]["messages"][0]["content"]["text"]
                .as_str()
                .ok_or_else(|| format!("prompts/get: {}", got.response))?;
            if !text.contains("This server serves all of them.") {
                run.failures
                    .push(format!("the prompt says this server lacks tools it names: {}", text.rsplit("\n\n").next().unwrap_or(text)));
            }
            let missing: Vec<String> = named_tools(text).into_iter().filter(|t| !plan.iter().any(|s| &s.tool == t)).collect();
            if !missing.is_empty() {
                run.failures.push(format!("the plan never calls tools the prompt names: {}", missing.join(", ")));
            }
        }
        for (n, step) in plan.iter().enumerate() {
            let ex = match &step.resource {
                Some(uri) => client.request("resources/read", json!({"uri": uri}))?,
                None => client.request("tools/call", json!({"name": step.tool, "arguments": step.args}))?,
            };
            run.calls += 1;
            tally(&mut run, &ex, false);
            let at = format!("step {} (`{}`)", n + 1, step.label());
            if let Some(error) = ex.response.get("error") {
                return Err(format!("{at}: refused: {}", error["message"]));
            }
            let text = if step.resource.is_some() { ex.resource_text() } else { ex.text() };
            if ex.response["result"]["isError"] == true {
                return Err(format!("{at}: failed: {text}"));
            }
            for want in strings(&step.expect["contains"]) {
                if !text.contains(&want) {
                    return Err(format!("{at}: its result lacks {want:?}: {text}"));
                }
            }
            seen.push_str(&text);
            seen.push('\n');
        }
        Ok(())
    };
    if let Err(e) = play() {
        run.failures.push(e);
    }
    client.finish();
    run.failures.extend(check_end(&task.end, &fake.world(), &seen));
    run.ms = started.elapsed().as_millis();
    run
}

fn run_task(task: &Task, setting: &'static str) -> Run {
    let toolsets = if setting == "all" { Toolsets::all() } else { task.setting() };
    run_plan(task, &task.plan, setting, toolsets)
}

/// The exact size of `tools/list`'s response for a server of `toolsets`, in bytes.
fn tools_list_bytes(toolsets: Toolsets) -> usize {
    let mut client = Client::start(Arc::new(Fake(Mutex::new(World::default()))), toolsets);
    let list = client.request("tools/list", json!({})).expect("tools/list");
    assert!(list.response["result"]["tools"].is_array(), "tools/list: {}", list.response);
    client.finish();
    list.in_bytes
}

// ---------------------------------------------------------------------------------------------
// The baseline and the table.

struct Baseline {
    core_bytes: usize,
    all_bytes: usize,
    /// Per task, per setting, its tokens.
    tasks: Map<String, Value>,
}

fn baseline_path() -> PathBuf {
    evals_dir().join("baseline.json")
}

fn read_baseline() -> Option<Baseline> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(baseline_path()).ok()?).ok()?;
    Some(Baseline {
        core_bytes: v["core_tools_list_bytes"].as_u64()? as usize,
        all_bytes: v["all_tools_list_bytes"].as_u64()? as usize,
        tasks: v["tasks"].as_object().cloned().unwrap_or_default(),
    })
}

fn write_baseline(core: usize, all: usize, runs: &[Run]) {
    let mut tasks = Map::new();
    for r in runs {
        let entry = tasks.entry(r.task.clone()).or_insert_with(|| json!({}));
        entry[r.setting] = r.tokens().into();
    }
    let v = json!({
        "about": "tests/evals.rs compares against this: the suite fails when the core tools/list grows more than 10% over \
                  core_tools_list_bytes. The tasks' tokens are for the table's comparison column. SIMEE_EVALS_UPDATE_BASELINE=1 \
                  cargo test -p simee-mcp --test evals rewrites it.",
        "core_tools_list_bytes": core,
        "all_tools_list_bytes": all,
        "tasks": tasks,
    });
    std::fs::write(baseline_path(), serde_json::to_string_pretty(&v).expect("json") + "\n").expect("write the baseline");
}

fn change(now: f64, was: f64) -> String {
    if was <= 0.0 { "new".into() } else { format!("{:+.1}%", (now - was) / was * 100.0) }
}

fn table(runs: &[Run], core: usize, all: usize, baseline: Option<&Baseline>) -> String {
    let mut out = String::from(
        "# MCP evals\n\n\
         Reference plans over the stdio server against the fake backend (`cargo test -p simee-mcp --test evals`; \
         docs/mcp-evals.md). Tokens are estimates: JSON bytes / 4, images width × height / 750.\n\n",
    );
    for (name, now, before) in [("core", core, baseline.map(|b| b.core_bytes)), ("all", all, baseline.map(|b| b.all_bytes))] {
        let vs = before.map_or_else(|| "no baseline".into(), |b| format!("baseline {b}, {}", change(now as f64, b as f64)));
        out.push_str(&format!("- `{name}` tools/list: {now} bytes, ≈{} tokens ({vs})\n", text_tokens(now)));
    }
    for setting in ["core", "all"] {
        let of: Vec<&Run> = runs.iter().filter(|r| r.setting == setting).collect();
        let passed = of.iter().filter(|r| r.ok()).count();
        let tokens: u64 = of.iter().map(|r| r.tokens()).sum();
        out.push_str(&format!("- `{setting}`: {passed}/{} tasks succeeded, ≈{tokens} tokens in all\n", of.len()));
    }
    out.push_str(
        "\n| Task | Setting | Toolsets | OK | Calls | Bytes out | Bytes in | tools/list ≈tok | Messages ≈tok | Total ≈tok | vs baseline | ms |\n\
         | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for r in runs {
        let before = baseline.and_then(|b| b.tasks.get(&r.task)).and_then(|t| t[r.setting].as_f64()).unwrap_or(0.0);
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.task,
            r.setting,
            r.toolsets,
            if r.ok() { "yes" } else { "**no**" },
            r.calls,
            r.out_bytes,
            r.in_bytes,
            r.list_tokens(),
            r.message_tokens,
            r.tokens(),
            change(r.tokens() as f64, before),
            r.ms
        ));
    }
    let failed: Vec<&Run> = runs.iter().filter(|r| !r.ok()).collect();
    if !failed.is_empty() {
        out.push_str("\n## Failures\n\n");
        for r in failed {
            out.push_str(&format!("- {} ({}): {}\n", r.task, r.setting, r.failures.join("; ")));
        }
    }
    out
}

fn write_result(name: &str, text: &str) -> PathBuf {
    let dir = out_dir();
    std::fs::create_dir_all(&dir).expect("target/mcp-evals");
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write the results");
    path
}

// ---------------------------------------------------------------------------------------------
// The suite.

#[test]
fn every_task_succeeds_and_the_core_tools_list_stays_within_its_baseline() {
    let tasks = load_tasks();
    for prompt in simee_mcp::prompts::PROMPTS {
        assert!(
            tasks.iter().any(|t| t.prompt.as_ref().is_some_and(|p| p["name"] == prompt.name)),
            "no task follows the `{}` prompt: add tests/evals/prompt-{}.json",
            prompt.name,
            prompt.name
        );
    }
    let mut runs = Vec::new();
    for task in &tasks {
        assert!(task.toolsets.split(',').next() == Some("core"), "{}: a task's toolsets start with core", task.name);
        runs.push(run_task(task, "core"));
        runs.push(run_task(task, "all"));
    }
    let (core, all) = (tools_list_bytes(Toolsets::default()), tools_list_bytes(Toolsets::all()));
    let update = std::env::var_os(UPDATE_ENV).is_some();
    if update {
        write_baseline(core, all, &runs);
    }
    let baseline = read_baseline();
    let report = table(&runs, core, all, baseline.as_ref());
    let path = write_result("results.md", &report);
    println!("{report}\n(written to {})", path.display());
    // In a GitHub Actions job, the table is the run's summary too.
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY").filter(|s| !s.is_empty()) {
        use std::io::Write;
        let appended = std::fs::OpenOptions::new().append(true).create(true).open(summary).and_then(|mut f| f.write_all(report.as_bytes()));
        if let Err(e) = appended {
            eprintln!("evals: cannot append to GITHUB_STEP_SUMMARY: {e}");
        }
    }

    let failed: Vec<String> =
        runs.iter().filter(|r| !r.ok()).map(|r| format!("{} ({}): {}", r.task, r.setting, r.failures.join("; "))).collect();
    assert!(failed.is_empty(), "tasks failed:\n{}", failed.join("\n"));
    let baseline = baseline.unwrap_or_else(|| panic!("tests/evals/baseline.json is missing or unreadable: {UPDATE_ENV}=1 writes it"));
    let limit = baseline.core_bytes as f64 * (1.0 + GROWTH);
    assert!(
        core as f64 <= limit,
        "the core tools/list is {core} bytes, {} over the baseline's {}: more than {:.0}% costs every agent's context on every \
         turn. Trim the descriptions or schemas, or, if the growth is worth it, rerun with {UPDATE_ENV}=1 and commit the baseline",
        change(core as f64, baseline.core_bytes as f64),
        baseline.core_bytes,
        GROWTH * 100.0
    );
}

/// The checker isn't vacuous: a plan that skips a step fails its end state.
#[test]
fn a_plan_missing_a_step_fails_its_end_state() {
    let tasks = load_tasks();
    let task = tasks.iter().find(|t| t.name == "boot-tablet-open-settings").expect("the task");
    let plan: Vec<Step> = task
        .plan
        .iter()
        .filter(|s| s.tool != "launch")
        .map(|s| Step { tool: s.tool.clone(), resource: s.resource.clone(), args: s.args.clone(), expect: s.expect.clone() })
        .collect();
    let run = run_plan(task, &plan, "core", task.setting());
    assert!(!run.ok());
    assert!(run.failures.iter().any(|f| f.contains("foreground")), "{:?}", run.failures);
    // A plan that boots the wrong device fails too.
    let wrong: Vec<Step> = task
        .plan
        .iter()
        .map(|s| Step { tool: s.tool.clone(), resource: None, args: json!({"device": "iphone-17"}), expect: Value::Null })
        .collect();
    assert!(!run_plan(task, &wrong, "core", task.setting()).ok());
}

/// A task that needs a tool outside the setting fails, refused by the server, never reaching the
/// backend; and a prompt task under too few toolsets is told so by its prompt.
#[test]
fn a_tool_outside_the_setting_is_refused() {
    let tasks = load_tasks();
    let fold = tasks.iter().find(|t| t.name == "foldable-tabletop").expect("the task");
    let run = run_plan(fold, &fold.plan, "core", Toolsets::default());
    assert!(run.failures.iter().any(|f| f.contains("refused") && f.contains("isn't enabled")), "{:?}", run.failures);
    let slow = tasks.iter().find(|t| t.name == "prompt-find-slowness").expect("the task");
    let run = run_plan(slow, &slow.plan, "core", Toolsets::default());
    assert!(run.failures.iter().any(|f| f.contains("lacks tools")), "{:?}", run.failures);
}

/// The measurements are the server's bytes: the `core` list is the pinned snapshot's tools, and
/// `all` is longer.
#[test]
fn the_tools_list_sizes_are_measured_from_the_server() {
    let (core, all) = (tools_list_bytes(Toolsets::default()), tools_list_bytes(Toolsets::all()));
    assert!(core > 10_000 && all > core * 2, "core {core}, all {all}");
    assert_eq!(core, tools_list_bytes(Toolsets::default()), "deterministic");
    assert_eq!(base64(b"simee"), "c2ltZWU=");
    assert_eq!(unbase64("c2ltZWU=").as_deref(), Some(&b"simee"[..]));
    assert_eq!(png_size(&fake_png(1206, 2622, "x")), Some((1206, 2622)));
    // A 1206×2622 screenshot scales to fit 1568 px tall: about 1 500 tokens, whatever its base64's length.
    assert_eq!(image_tokens(1206, 2622), 1_508);
}

// ---------------------------------------------------------------------------------------------
// Live mode: each goal handed to an external agent over Streamable HTTP. Never run in CI.

/// Reads the optional `simee-evals-tokens: <n>` line an agent prints.
fn reported_tokens(stdout: &str) -> Option<u64> {
    stdout.lines().rev().find_map(|l| l.trim().strip_prefix("simee-evals-tokens:").and_then(|n| n.trim().parse().ok()))
}

#[test]
fn live_agent() {
    let Some(agent) = std::env::var(AGENT_ENV).ok().filter(|a| !a.trim().is_empty()) else {
        return;
    };
    let timeout = Duration::from_secs(std::env::var("SIMEE_EVALS_TIMEOUT_S").ok().and_then(|s| s.parse().ok()).unwrap_or(600));
    let mut report = format!(
        "# MCP evals, live\n\nAgent: `{agent}`. Calls are the backend's; tokens are what the agent reported (`simee-evals-tokens:`).\n\n\
         | Task | Toolsets | OK | Calls | Tokens | s | Failures |\n| --- | --- | --- | ---: | ---: | ---: | --- |\n"
    );
    let tasks = load_tasks();
    let mut passed = 0;
    for task in &tasks {
        let fake = Arc::new(Fake(Mutex::new(World::from_json(&task.world).expect("world"))));
        let mut config = simee_mcp::http::Config::new("127.0.0.1:0".parse().expect("addr"));
        config.no_auth = true;
        config.toolsets = task.setting();
        let server = simee_mcp::http::start(fake.clone(), config).expect("serve over HTTP");
        let url = format!("http://{}{}", server.addr(), simee_mcp::http::ENDPOINT);
        let prompt = task.prompt.as_ref().map(Value::to_string).unwrap_or_default();
        let started = Instant::now();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&agent)
            .env("SIMEE_EVALS_TASK", &task.name)
            .env("SIMEE_EVALS_GOAL", &task.goal)
            .env("SIMEE_EVALS_PROMPT", &prompt)
            .env("SIMEE_EVALS_MCP_URL", &url)
            .env("SIMEE_EVALS_TOOLSETS", &task.toolsets)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot run {AGENT_ENV} `{agent}`: {e}"));
        let mut stdout = child.stdout.take().expect("stdout");
        let reader = std::thread::spawn(move || {
            let mut out = String::new();
            let _ = stdout.read_to_string(&mut out);
            out
        });
        let mut failures = Vec::new();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => failures.push(format!("the agent exited with {status}")),
                Ok(Some(_)) => {}
                Ok(None) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    failures.push(format!("the agent ran past {timeout:?}"));
                }
                Err(e) => failures.push(format!("waiting for the agent: {e}")),
            }
            break;
        }
        let said = reader.join().unwrap_or_default();
        drop(server);
        let world = fake.world();
        failures.extend(check_end(&task.end, &world, &said));
        let ok = failures.is_empty();
        passed += usize::from(ok);
        report.push_str(&format!(
            "| {} | {} | {} | {} | {} | {:.1} | {} |\n",
            task.name,
            task.toolsets,
            if ok { "yes" } else { "**no**" },
            world.calls.len(),
            reported_tokens(&said).map_or_else(|| "-".into(), |t| t.to_string()),
            started.elapsed().as_secs_f64(),
            failures.join("; ")
        ));
    }
    report.push_str(&format!("\n{passed}/{} tasks succeeded.\n", tasks.len()));
    let path = write_result("live.md", &report);
    println!("{report}\n(written to {})", path.display());
}
