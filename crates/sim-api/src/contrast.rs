//! `contrast_check`: a screenshot's text and controls below WCAG contrast, from pixels alone.
//!
//! The analysis is `sim_core::contrast`; this is the method around it. Each backend supplies a
//! [`Target`] (a capture, a scale, an appearance switch) and [`run`] shapes the parameters and the
//! result the same way for both, so a test drives it with a fake device.

use std::time::Duration;

use serde_json::{Value, json};
use sim_core::contrast::{self, Options, Rect, Region, Report};
use sim_core::frame::{Frame, encode_png};

use crate::params::{opt_u64, png_result, str_param};

/// Light or dark UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }
}

/// What one device gives `contrast_check`.
pub trait Target {
    /// The screen now.
    fn capture(&mut self) -> Result<Frame, String>;
    /// Pixels per point (iOS's scale, Android's density over 160), when the device knows.
    fn scale(&mut self) -> Option<f64>;
    /// Switches the UI to `mode`.
    fn set_appearance(&mut self, mode: Mode) -> Result<(), String>;
}

/// How long the UI gets to redraw after an appearance switch.
const SETTLE_MS: u64 = 500;
/// The longest `settle_ms` taken.
const MAX_SETTLE_MS: u64 = 10_000;
/// The largest `scale` taken (pixels per point).
const MAX_SCALE: f64 = 16.0;

/// A region of the screen in pixels: `{x, y, width, height}`.
fn region(p: &Value) -> Result<Option<Rect>, String> {
    let Some(r) = p.get("region").filter(|r| !r.is_null()) else { return Ok(None) };
    let field = |k: &str| {
        r.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("`region` needs a whole number `{k}`: {{x, y, width, height}} in pixels"))
    };
    Ok(Some(Rect { x: field("x")?, y: field("y")?, width: field("width")?, height: field("height")? }))
}

fn mode(p: &Value, key: &str) -> Result<Option<Mode>, String> {
    match str_param(p, key) {
        None => Ok(None),
        Some("light") => Ok(Some(Mode::Light)),
        Some("dark") => Ok(Some(Mode::Dark)),
        Some(other) => Err(format!("unknown {key} `{other}` (light or dark)")),
    }
}

/// A ratio to 2 places, never rounded up across the line a finding is under (4.499 is not 4.50).
fn ratio(r: &Region) -> f64 {
    let shown = (r.ratio * 100.0).round() / 100.0;
    if r.fails() && shown >= r.kind.required() { r.kind.required() - 0.01 } else { shown }
}

fn finding(r: &Region) -> Value {
    json!({
        "x": r.rect.x, "y": r.rect.y, "width": r.rect.width, "height": r.rect.height,
        "kind": r.kind.as_str(),
        "foreground": contrast::hex(r.foreground), "background": contrast::hex(r.background),
        "ratio": ratio(r), "required": r.kind.required(),
    })
}

/// One screen's check: its findings and the annotated PNG (`png`, or written to `path`).
fn check(frame: &Frame, opts: &Options, path: Option<String>) -> Result<(Report, Value), String> {
    let report = contrast::check(frame, opts);
    let findings: Vec<Value> = report.findings().map(finding).collect();
    let png = encode_png(&contrast::annotate(frame, report.findings()))?;
    let mut p = json!({});
    if let Some(path) = path {
        p["path"] = Value::String(path);
    }
    let mut result = png_result(frame.width, frame.height, png, &p)?;
    result["checked"] = report.regions.len().into();
    result["failing"] = findings.len().into();
    result["findings"] = Value::Array(findings);
    Ok((report, result))
}

/// `name.png` as `name-light.png`: where one appearance's PNG goes when both are checked.
fn beside(path: &str, mode: Mode) -> String {
    let p = std::path::Path::new(path);
    let stem = p.file_stem().map_or_else(|| "contrast".into(), |s| s.to_string_lossy().into_owned());
    let name = match p.extension() {
        Some(ext) => format!("{stem}-{}.{}", mode.as_str(), ext.to_string_lossy()),
        None => format!("{stem}-{}", mode.as_str()),
    };
    p.with_file_name(name).to_string_lossy().into_owned()
}

/// Whether to check both appearances (`appearance`: current, the default, or both), the one to
/// leave the device in (`restore`, default light) and the wait after each switch (`settle_ms`,
/// default [`SETTLE_MS`]).
pub(crate) fn sweep_options(p: &Value) -> Result<(bool, Mode, Duration), String> {
    let both = match str_param(p, "appearance") {
        None | Some("current") => false,
        Some("both") => true,
        Some(other) => return Err(format!("unknown appearance `{other}` (both, or omit it to check the screen as it is)")),
    };
    let restore = mode(p, "restore")?.unwrap_or(Mode::Light);
    let settle_ms = opt_u64(p, "settle_ms").unwrap_or(SETTLE_MS);
    if settle_ms > MAX_SETTLE_MS {
        return Err(format!("`settle_ms` must be at most {MAX_SETTLE_MS}"));
    }
    Ok((both, restore, Duration::from_millis(settle_ms)))
}

/// `contrast_check`: the screen now, or with `appearance: both` the light and the dark UI (switch,
/// wait `settle_ms`, check; then back to `restore`).
pub fn run(t: &mut impl Target, p: &Value) -> Result<Value, String> {
    let (both, restore, settle) = sweep_options(p)?;
    let (scale, scale_source) = match p.get("scale").and_then(Value::as_f64) {
        Some(s) if s.is_finite() && s > 0.0 && s <= MAX_SCALE => (s, "param"),
        Some(s) if s.is_finite() && s > MAX_SCALE => return Err(format!("`scale` must be at most {MAX_SCALE} (pixels per point)")),
        Some(_) => return Err("`scale` must be a positive number (pixels per point)".into()),
        None => t.scale().filter(|s| s.is_finite() && *s > 0.0).map_or((1.0, "assumed"), |s| (s, "device")),
    };
    let opts = Options { region: region(p)?, scale };
    let path = str_param(p, "path");
    let tag = |mut r: Value| {
        r["scale"] = json!(scale);
        r["scale_source"] = json!(scale_source);
        r
    };
    if !both {
        let frame = t.capture()?;
        return Ok(tag(check(&frame, &opts, path.map(str::to_owned))?.1));
    }
    let mut appearances = serde_json::Map::new();
    let mut failing = 0;
    let mut sweep = || -> Result<(), String> {
        for m in [Mode::Light, Mode::Dark] {
            t.set_appearance(m)?;
            std::thread::sleep(settle);
            let frame = t.capture()?;
            let (report, result) = check(&frame, &opts, path.map(|p| beside(p, m)))?;
            failing += report.findings().count();
            appearances.insert(m.as_str().to_owned(), result);
        }
        Ok(())
    };
    let swept = sweep();
    // Back to where the caller wants it, whatever happened in between.
    let restored = t.set_appearance(restore);
    swept?;
    restored?;
    std::thread::sleep(settle);
    Ok(tag(json!({"appearances": appearances, "failing": failing, "restored": restore.as_str()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A device whose screen is a light frame or a dark frame by appearance, and that records the
    /// appearances it was put in.
    struct Fake {
        mode: Rc<RefCell<Vec<Mode>>>,
        scale: Option<f64>,
        switch: bool,
    }

    fn text(width: u32, height: u32, bg: [u8; 3], fg: [u8; 3]) -> Frame {
        let mut f = Frame { width, height, rgba: (0..width * height).flat_map(|_| [bg[0], bg[1], bg[2], 255]).collect() };
        // Letters as hollow boxes with 4 px strokes, spaced like a word.
        for x0 in [40, 64, 88, 112] {
            for y in 50..82 {
                for x in x0..x0 + 16 {
                    if !(54..78).contains(&y) || !(x0 + 4..x0 + 12).contains(&x) {
                        let i = ((y * width + x) * 4) as usize;
                        f.rgba[i..i + 3].copy_from_slice(&fg);
                    }
                }
            }
        }
        f
    }

    impl Target for Fake {
        fn capture(&mut self) -> Result<Frame, String> {
            Ok(match self.mode.borrow().last() {
                Some(Mode::Dark) => text(200, 120, [0, 0, 0], [0x44, 0x44, 0x44]),
                _ => text(200, 120, [255, 255, 255], [0x77, 0x77, 0x77]),
            })
        }

        fn scale(&mut self) -> Option<f64> {
            self.scale
        }

        fn set_appearance(&mut self, mode: Mode) -> Result<(), String> {
            if self.switch {
                self.mode.borrow_mut().push(mode);
                Ok(())
            } else {
                Err("appearance switching isn't available on Android devices yet".into())
            }
        }
    }

    fn fake(scale: Option<f64>, switch: bool) -> (Fake, Rc<RefCell<Vec<Mode>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        (Fake { mode: log.clone(), scale, switch }, log)
    }

    #[test]
    fn a_screen_is_checked_as_it_is() {
        let (mut t, log) = fake(Some(2.0), true);
        let r = run(&mut t, &json!({})).expect("check");
        assert!(log.borrow().is_empty(), "no appearance change without `appearance`");
        assert_eq!((r["width"].as_u64(), r["height"].as_u64()), (Some(200), Some(120)));
        assert_eq!((r["scale"].as_f64(), r["scale_source"].as_str()), (Some(2.0), Some("device")));
        assert_eq!(r["failing"], 1, "{r}");
        let f = &r["findings"][0];
        assert_eq!((f["foreground"].as_str(), f["background"].as_str()), (Some("#777777"), Some("#ffffff")));
        assert_eq!((f["required"].as_f64(), f["kind"].as_str()), (Some(4.5), Some("text")));
        // #777 on white is 4.478:1: shown as 4.48, which is still under 4.5.
        assert_eq!(f["ratio"].as_f64(), Some(4.48));
        assert!(r["png"].is_string(), "the annotated PNG comes back in `png`");
    }

    #[test]
    fn a_ratio_just_under_the_line_never_prints_as_the_line() {
        let r = Region {
            rect: Rect { x: 0, y: 0, width: 1, height: 1 },
            kind: sim_core::contrast::Kind::Text,
            foreground: [0; 3],
            background: [0; 3],
            ratio: 4.4996,
        };
        assert_eq!(ratio(&r), 4.49);
        assert_eq!(ratio(&Region { ratio: 4.5, ..r }), 4.5);
        assert_eq!(ratio(&Region { ratio: 3.1049, ..r }), 3.1);
    }

    #[test]
    fn the_scale_comes_from_the_call_then_the_device_then_one() {
        let (mut t, _) = fake(Some(3.0), true);
        assert_eq!(run(&mut t, &json!({"scale": 2})).expect("param")["scale_source"], "param");
        assert_eq!(run(&mut t, &json!({})).expect("device")["scale"], 3.0);
        let (mut t, _) = fake(None, true);
        let r = run(&mut t, &json!({})).expect("assumed");
        assert_eq!((r["scale"].as_f64(), r["scale_source"].as_str()), (Some(1.0), Some("assumed")));
        assert!(run(&mut t, &json!({"scale": 0})).is_err());
        assert!(run(&mut t, &json!({"scale": -1})).is_err());
    }

    #[test]
    fn both_appearances_are_checked_and_the_device_is_put_back() {
        let (mut t, log) = fake(Some(2.0), true);
        let r = run(&mut t, &json!({"appearance": "both", "settle_ms": 0, "restore": "dark"})).expect("both");
        assert_eq!(*log.borrow(), [Mode::Light, Mode::Dark, Mode::Dark], "light, dark, then back to `restore`");
        assert_eq!(r["restored"], "dark");
        let (light, dark) = (&r["appearances"]["light"], &r["appearances"]["dark"]);
        assert_eq!(light["findings"][0]["foreground"], "#777777");
        assert_eq!(dark["findings"][0]["background"], "#000000");
        assert_eq!(dark["findings"][0]["foreground"], "#444444");
        assert_eq!(r["failing"], 2);
        assert!(light["png"].is_string() && dark["png"].is_string());
        // The default `restore` is light, the simulator's own default.
        let (mut t, log) = fake(Some(2.0), true);
        assert_eq!(run(&mut t, &json!({"appearance": "both", "settle_ms": 0})).expect("both")["restored"], "light");
        assert_eq!(log.borrow().last(), Some(&Mode::Light));
    }

    #[test]
    fn a_device_that_cannot_switch_says_so_and_checks_nothing() {
        let (mut t, log) = fake(Some(2.0), false);
        let err = run(&mut t, &json!({"appearance": "both", "settle_ms": 0})).expect_err("no switching");
        assert_eq!(err, "appearance switching isn't available on Android devices yet");
        assert!(log.borrow().is_empty());
        // The screen as it is still works there.
        assert_eq!(run(&mut t, &json!({})).expect("current")["failing"], 1);
    }

    #[test]
    fn a_region_limits_the_check_and_is_validated() {
        let (mut t, _) = fake(Some(2.0), true);
        let miss = json!({"region": {"x": 0, "y": 100, "width": 200, "height": 20}});
        let r = run(&mut t, &miss).expect("below the text");
        assert_eq!((r["checked"].as_u64(), r["failing"].as_u64()), (Some(0), Some(0)));
        let hit = json!({"region": {"x": 0, "y": 0, "width": 200, "height": 100}});
        assert_eq!(run(&mut t, &hit).expect("over the text")["failing"], 1);
        let err = run(&mut t, &json!({"region": {"x": 0, "y": 0, "width": 5}})).expect_err("missing height");
        assert!(err.contains("`height`"), "{err}");
        assert!(run(&mut t, &json!({"region": {"x": -1, "y": 0, "width": 5, "height": 5}})).is_err());
    }

    #[test]
    fn bad_parameters_are_refused() {
        let (mut t, log) = fake(Some(2.0), true);
        assert!(run(&mut t, &json!({"appearance": "dark"})).expect_err("only both").contains("unknown appearance `dark`"));
        assert!(run(&mut t, &json!({"appearance": "both", "restore": "sepia"})).is_err());
        assert!(log.borrow().is_empty(), "refused before touching the device");
    }

    #[test]
    fn scale_and_settle_are_bounded() {
        let (mut t, log) = fake(Some(2.0), true);
        for bad in [json!({"scale": 17}), json!({"scale": 1e300}), json!({"scale": 16.5})] {
            assert!(run(&mut t, &bad).expect_err("scale too large").contains("at most 16"), "{bad}");
        }
        assert_eq!(run(&mut t, &json!({"scale": 16})).expect("16 is fine")["scale"], 16.0);
        let err = run(&mut t, &json!({"appearance": "both", "settle_ms": 10001})).expect_err("settle too long");
        assert!(err.contains("at most 10000"), "{err}");
        assert!(log.borrow().is_empty(), "refused before touching the device");
    }

    #[test]
    fn the_annotated_png_goes_to_path_when_given() {
        let dir = std::env::temp_dir().join(format!("sim-api-contrast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("shot.png");
        let (mut t, _) = fake(Some(2.0), true);
        let r = run(&mut t, &json!({"path": path.to_str()})).expect("check");
        assert_eq!(r["path"], path.to_str().expect("utf-8"));
        assert!(r.get("png").is_none());
        let png = std::fs::read(&path).expect("written");
        assert_eq!(sim_core::frame::png_size(&png), Some((200, 120)));
        // Both appearances: a file each, named for the appearance.
        let r = run(&mut t, &json!({"path": path.to_str(), "appearance": "both", "settle_ms": 0})).expect("both");
        for m in ["light", "dark"] {
            let file = dir.join(format!("shot-{m}.png"));
            assert_eq!(r["appearances"][m]["path"], file.to_str().expect("utf-8"));
            assert!(file.exists());
        }
        std::fs::remove_dir_all(dir).ok();
        // The host's own separator joins the new name (a `\` on Windows).
        assert_eq!(beside("/a/b/c", Mode::Dark), std::path::Path::new("/a/b").join("c-dark").to_string_lossy());
    }

    #[test]
    fn appearance_is_current_or_both_in_the_method_table() {
        let (_, params, doc) = crate::METHODS.iter().find(|(name, ..)| *name == "contrast_check").expect("contrast_check is a method");
        assert!(params.contains("appearance?(current|both)"), "{params}");
        assert!(doc.contains("appearance both"), "{doc}");
    }
}
