//! Parameter parsing and result shaping shared by every backend, so a method means the same on
//! iOS and Android.

use std::borrow::Cow;
use std::cell::RefCell;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use sim_core::form_factor::FormFactor;
#[cfg(target_os = "macos")]
use sim_core::frame::{Frame, encode_png};
use sim_core::geometry::Point;
use sim_core::gesture::{self, Phase, Sample, ScreenEdge};
use sim_core::hid::{Button, CROWN_UNITS_PER_TICK, Orientation};
use sim_core::indigo::PointerButton;
use sim_core::platform::Platform;
use sim_core::pointer::PointerAction;
use sim_core::{stylus, vhal};

pub fn str_param<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

pub fn need_str<'a>(p: &'a Value, key: &str) -> Result<&'a str, String> {
    str_param(p, key).ok_or_else(|| format!("missing string parameter `{key}`"))
}

pub fn need_f64(p: &Value, key: &str) -> Result<f64, String> {
    p.get(key).and_then(Value::as_f64).ok_or_else(|| format!("missing number parameter `{key}`"))
}

pub fn opt_u64(p: &Value, key: &str) -> Option<u64> {
    p.get(key).and_then(Value::as_u64)
}

/// `boot_many`'s `concurrency`: at least 1; by default a quarter of the host's CPUs, at least 2.
pub fn concurrency(p: &Value) -> Result<usize, String> {
    match opt_u64(p, "concurrency") {
        Some(0) => Err("concurrency must be at least 1".into()),
        Some(k) => Ok(k as usize),
        None => Ok(crate::settle::default_concurrency(std::thread::available_parallelism().map_or(4, usize::from))),
    }
}

/// An optional boolean: absent or null is `None`, any other type is refused.
pub fn opt_bool(p: &Value, key: &str) -> Result<Option<bool>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(format!("`{key}` must be true or false, not {other}")),
    }
}

// Only iOS methods read this so far.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn need_bool(p: &Value, key: &str) -> Result<bool, String> {
    p.get(key).and_then(Value::as_bool).ok_or_else(|| format!("missing boolean parameter `{key}`"))
}

pub fn strings(p: &Value, key: &str) -> Vec<String> {
    p.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

// Only iOS methods read this so far (Android's exec refuses a value that isn't a string).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn pairs(p: &Value, key: &str) -> Vec<(String, String)> {
    p.get(key)
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect())
        .unwrap_or_default()
}

pub fn err<E: ToString>(e: E) -> String {
    e.to_string()
}

/// The optional `platform` parameter.
pub fn opt_platform(p: &Value) -> Result<Option<Platform>, String> {
    str_param(p, "platform").map(|v| Platform::parse(v).ok_or_else(|| format!("unknown platform `{v}` (android or ios)"))).transpose()
}

/// The optional `form_factor` parameter.
pub fn opt_form_factor(p: &Value) -> Result<Option<FormFactor>, String> {
    str_param(p, "form_factor")
        .map(|v| {
            FormFactor::parse(v).ok_or_else(|| {
                let names: Vec<&str> = FormFactor::ALL.iter().map(|f| f.as_str()).collect();
                format!("unknown form factor `{v}` ({})", names.join(", "))
            })
        })
        .transpose()
}

/// `tap`: x, y and hold_ms as touch samples in device pixels; without hold_ms the touch is held
/// `default_hold`, which each platform sets (Android 60 ms, iOS 10 ms).
pub fn tap_samples(p: &Value, default_hold: Duration) -> Result<Vec<Sample>, String> {
    let at = Point::new(need_f64(p, "x")?, need_f64(p, "y")?);
    Ok(gesture::tap(at, opt_u64(p, "hold_ms").map_or(default_hold, Duration::from_millis)))
}

/// `swipe`: x1, y1, x2, y2, duration_ms (default 300) and settle as touch samples.
pub fn swipe_samples(p: &Value) -> Result<Vec<Sample>, String> {
    let from = Point::new(need_f64(p, "x1")?, need_f64(p, "y1")?);
    let to = Point::new(need_f64(p, "x2")?, need_f64(p, "y2")?);
    let duration = Duration::from_millis(opt_u64(p, "duration_ms").unwrap_or(300));
    let settle = p.get("settle").and_then(Value::as_bool).unwrap_or(false);
    Ok(gesture::swipe(from, to, duration, Duration::from_millis(17), settle))
}

/// `touch`: one sample's point and phase.
pub fn touch_sample(p: &Value) -> Result<(Point, Phase), String> {
    let phase = match need_str(p, "phase")? {
        "down" => Phase::Down,
        "move" => Phase::Move,
        "up" => Phase::Up,
        other => return Err(format!("unknown phase `{other}`")),
    };
    Ok((Point::new(need_f64(p, "x")?, need_f64(p, "y")?), phase))
}

/// `button`: the button and how long to hold it (hold_ms; without it `default_hold`, which each
/// platform sets).
pub fn button(p: &Value, default_hold: Duration) -> Result<(Button, Duration), String> {
    let name = need_str(p, "button")?;
    let button = Button::parse(name).ok_or_else(|| format!("unknown button `{name}`"))?;
    Ok((button, opt_u64(p, "hold_ms").map_or(default_hold, Duration::from_millis)))
}

/// The optional `orientation` parameter.
pub fn opt_orientation(p: &Value) -> Result<Option<Orientation>, String> {
    str_param(p, "orientation")
        .map(|v| {
            Orientation::parse(v).ok_or_else(|| {
                let names: Vec<&str> = Orientation::ALL.iter().map(|o| o.as_str()).collect();
                format!("unknown orientation `{v}` ({})", names.join(", "))
            })
        })
        .transpose()
}

/// The screen the optional `display` parameter names: the one in use (`main`, the default), an
/// iPhone Duo's `outer` or `inner` screen, or an iPhone's CarPlay screen (`carplay`).
pub fn display(p: &Value) -> Result<sim_core::duo::Display, String> {
    sim_core::duo::Display::parse(str_param(p, "display"))
}

/// `remote_swipe` (iOS only, so unused where the iOS backend isn't built): a swipe across the Siri Remote's touch surface in `direction`, as samples
/// normalized to the surface (`0..=1`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn remote_swipe_samples(p: &Value) -> Result<Vec<Sample>, String> {
    let (from, to) = match need_str(p, "direction")? {
        "up" => (Point::new(0.5, 0.8), Point::new(0.5, 0.2)),
        "down" => (Point::new(0.5, 0.2), Point::new(0.5, 0.8)),
        "left" => (Point::new(0.8, 0.5), Point::new(0.2, 0.5)),
        "right" => (Point::new(0.2, 0.5), Point::new(0.8, 0.5)),
        other => return Err(format!("unknown direction `{other}` (up, down, left, right)")),
    };
    let duration = Duration::from_millis(opt_u64(p, "duration_ms").unwrap_or(250));
    Ok(gesture::swipe(from, to, duration, Duration::from_millis(17), false))
}

/// `swipe`'s and `touch`'s optional `edge` (iOS): the screen edge the touch comes in from.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn opt_edge(p: &Value) -> Result<Option<ScreenEdge>, String> {
    str_param(p, "edge").map(|v| ScreenEdge::parse(v).ok_or_else(|| format!("unknown edge `{v}` ({})", ScreenEdge::NAMES))).transpose()
}

/// `stylus`'s samples on an iPad (the Apple Pencil), in pixels: a stroke from x, y to to_x, to_y
/// over duration_ms (300 by default), else one phase at x, y (`tap` by default: down, then up
/// 60 ms later). Hover, pressure, the barrel button and the eraser, which Android's stylus takes,
/// are refused with the reason: the simulator's touch screen has none of them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn pencil_samples(p: &Value) -> Result<Vec<Sample>, String> {
    const NONE: &str = "the simulator's touch screen reports only the Apple Pencil's position (docs/form-factors.md)";
    if p.get("pressure").is_some_and(|v| !v.is_null()) {
        return Err(format!("an iPad's stylus has no `pressure`: {NONE}"));
    }
    for key in ["button", "eraser"] {
        if opt_bool(p, key)? == Some(true) {
            return Err(format!("an iPad's stylus has no `{key}`: {NONE}"));
        }
    }
    let at = Point::new(need_f64(p, "x")?, need_f64(p, "y")?);
    let to = (p.get("to_x").and_then(Value::as_f64), p.get("to_y").and_then(Value::as_f64));
    match to {
        (Some(tx), Some(ty)) => {
            let duration = Duration::from_millis(opt_u64(p, "duration_ms").unwrap_or(300));
            Ok(gesture::swipe(at, Point::new(tx, ty), duration, Duration::from_millis(17), false))
        }
        (None, None) => {
            let phase = match str_param(p, "phase").unwrap_or("tap") {
                "tap" => return Ok(gesture::tap(at, Duration::from_millis(opt_u64(p, "hold_ms").unwrap_or(60)))),
                "down" => Phase::Down,
                "move" => Phase::Move,
                "up" => Phase::Up,
                phase @ ("hover" | "leave") => {
                    return Err(format!("an iPad's stylus can't `{phase}` (it takes tap, down, move, up): {NONE}"));
                }
                other => return Err(format!("unknown phase `{other}` (tap, down, move, up)")),
            };
            Ok(vec![Sample { point: at, phase, delay: Duration::ZERO }])
        }
        _ => Err("a stroke needs both `to_x` and `to_y`".into()),
    }
}

/// What a `pointer` call asks for, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerRequest {
    pub at: Point,
    /// What to do at `at` (`move` by default).
    pub action: PointerAction,
    pub button: PointerButton,
    /// How long a click holds the button.
    pub hold: Duration,
    /// A drag: press at `at`, move here holding the button, release.
    pub drag_to: Option<Point>,
}

/// `pointer`'s request: x, y, then an `action` there (`move` by default) or a drag to to_x, to_y;
/// the button (`primary` by default) and how long a click holds it (80 ms by default).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn pointer_request(p: &Value) -> Result<PointerRequest, String> {
    let at = Point::new(need_f64(p, "x")?, need_f64(p, "y")?);
    let drag_to = match (p.get("to_x").and_then(Value::as_f64), p.get("to_y").and_then(Value::as_f64)) {
        (Some(x), Some(y)) => Some(Point::new(x, y)),
        (None, None) => None,
        _ => return Err("a drag needs both `to_x` and `to_y`".into()),
    };
    let action = match (str_param(p, "action"), drag_to) {
        (Some(a), Some(_)) => return Err(format!("a drag (`to_x`, `to_y`) takes no `action` (got `{a}`)")),
        (a, _) => a.unwrap_or("move"),
    };
    let action = PointerAction::parse(action).ok_or_else(|| format!("unknown action `{action}` ({})", PointerAction::NAMES))?;
    let button = str_param(p, "button").unwrap_or("primary");
    let button = PointerButton::parse(button).ok_or_else(|| format!("unknown button `{button}` ({})", PointerButton::NAMES))?;
    Ok(PointerRequest { at, action, button, hold: Duration::from_millis(opt_u64(p, "hold_ms").unwrap_or(80)), drag_to })
}

/// `rotary`'s turn on an Apple Watch, in Digital Crown units: `delta` as given, else `ticks` detents.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn crown_delta(p: &Value) -> Result<f64, String> {
    match (p.get("delta").and_then(Value::as_f64), p.get("ticks").and_then(Value::as_i64)) {
        (Some(delta), _) => Ok(delta),
        (None, Some(ticks)) => Ok(ticks as f64 * CROWN_UNITS_PER_TICK),
        (None, None) => Err("give `ticks` (detents) or `delta` (Digital Crown units)".into()),
    }
}

/// `vehicle`'s `area_id`: a number, or a string in decimal or hex (`"0x31"`); default 0, the
/// global area.
pub fn vehicle_area(p: &Value) -> Result<i32, String> {
    match p.get("area_id") {
        None | Some(Value::Null) => Some(0),
        Some(Value::Number(n)) => n.as_i64().and_then(|v| vhal::parse_id(&v.to_string())),
        Some(Value::String(s)) => vhal::parse_id(s),
        Some(_) => None,
    }
    .ok_or_else(|| "`area_id` must be a 32-bit number".into())
}

/// `vehicle`'s `value` as the console's text: strings as given, numbers and booleans written
/// out, arrays comma-separated.
pub fn vehicle_value(p: &Value) -> Result<Option<String>, String> {
    let text = |v: &Value| match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(_) | Value::Bool(_) => Ok(v.to_string()),
        _ => Err("`value` must be a string, number, boolean or an array of numbers".to_string()),
    };
    match p.get("value") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items.iter().map(text).collect::<Result<Vec<_>, _>>().map(|t| Some(t.join(","))),
        Some(v) => text(v).map(Some),
    }
}

/// `stylus`'s samples and how long to spread them over: a stroke with `to_x`/`to_y` (300 ms by
/// default), else one phase at a point, `tap` (down, up, out of range) by default.
pub fn stylus_samples(p: &Value) -> Result<(Vec<stylus::PenSample>, Duration), String> {
    let coord = |k: &str| -> Result<Option<i32>, String> {
        p.get(k)
            .map(|v| v.as_u64().and_then(|n| i32::try_from(n).ok()).ok_or(format!("`{k}` must be a whole number of pixels")))
            .transpose()
    };
    let at = |k: &str| coord(k)?.ok_or(format!("missing number parameter `{k}`"));
    let (x, y) = (at("x")?, at("y")?);
    let (button, eraser) = (opt_bool(p, "button")?.unwrap_or(false), opt_bool(p, "eraser")?.unwrap_or(false));
    let pressure = match p.get("pressure") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|n| u8::try_from(n).ok()).filter(|n| *n > 0).ok_or("`pressure` must be 1-255")?),
    };
    match (coord("to_x")?, coord("to_y")?) {
        (Some(tx), Some(ty)) => {
            let ms = opt_u64(p, "duration_ms").unwrap_or(300);
            let mut samples = stylus::stroke((x, y), (tx, ty), 10, button, eraser);
            samples.iter_mut().filter(|s| s.phase != stylus::PenPhase::Leave).for_each(|s| s.pressure = pressure);
            Ok((samples, Duration::from_millis(ms)))
        }
        (None, None) => {
            let mut samples = match str_param(p, "phase").unwrap_or("tap") {
                "tap" => stylus::tap(x, y, button, eraser),
                name => {
                    let phase = stylus::PenPhase::parse(name)
                        .ok_or_else(|| format!("unknown phase `{name}` (tap, {})", stylus::PenPhase::NAMES))?;
                    vec![stylus::PenSample { x, y, phase, pressure: None, button, eraser }]
                }
            };
            samples.iter_mut().for_each(|s| s.pressure = pressure);
            Ok((samples, Duration::ZERO))
        }
        _ => Err("a stroke needs both `to_x` and `to_y`".into()),
    }
}

/// A vehicle property reading as JSON, its value typed by the property (booleans, numbers,
/// arrays of numbers; strings and bytes stay text).
pub fn reading_json(r: &vhal::Reading) -> Value {
    use vhal::PropType as T;
    let num = |s: &str| -> Value {
        s.parse::<i64>().map(Value::from).or_else(|_| s.parse::<f64>().map(Value::from)).unwrap_or_else(|_| s.into())
    };
    let value = match r.prop_type() {
        T::Boolean => Value::Bool(r.value == "true"),
        T::Int32 | T::Int64 | T::Float => num(&r.value),
        T::Int32Vec | T::Int64Vec | T::FloatVec => Value::Array(r.value.split(',').filter(|s| !s.is_empty()).map(num).collect()),
        _ => Value::String(r.value.clone()),
    };
    json!({"property": r.property, "id": format!("{:#010x}", r.id as u32), "area_id": r.area_id, "status": r.status, "value": value})
}

/// `screenshot`'s result: written to `path`, else the PNG inline (see [`png_result`]).
#[cfg(target_os = "macos")]
pub fn screenshot_result(frame: &Frame, p: &Value) -> Result<Value, String> {
    png_result(frame.width, frame.height, encode_png(frame)?, p)
}

/// An encoded screenshot written to `path`, or inline without one: base64 in `png`, or, while
/// the caller takes raw bytes ([`taking_bytes`]), `png` holding `{"binary": <len>}` and the bytes
/// moved beside the result (an owned `png` moves without a copy).
pub fn png_result<'a>(width: u32, height: u32, png: impl Into<Cow<'a, [u8]>>, p: &Value) -> Result<Value, String> {
    let png = png.into();
    match str_param(p, "path") {
        Some(path) => std::fs::write(path, &png).map(|()| json!({"path": path, "width": width, "height": height})).map_err(err),
        None => {
            let mut result = json!({"width": width, "height": height});
            result["png"] = bytes_field(png);
            Ok(result)
        }
    }
}

thread_local! {
    /// `Some` while a caller on this thread takes raw bytes ([`taking_bytes`]), holding the bytes
    /// a result moved out (one field's: a reply carries at most one attachment).
    static TAKEN: RefCell<Option<Option<Vec<u8>>>> = const { RefCell::new(None) };
}

/// Runs `f` (one method call) with this thread taking raw bytes: the first bytes field its result
/// would carry as base64 (`png_result`'s `png`) holds `{"binary": <len>}` instead, and the bytes
/// come back beside the result. Only results built on this thread are taken; any other caller
/// (MCP, a call on another thread) gets base64 as before. Not reentrant: a nested call would end
/// the outer one's taking (nothing nests today).
pub fn taking_bytes<R>(f: impl FnOnce() -> R) -> (R, Option<Vec<u8>>) {
    /// Stops the thread taking bytes however `f` ends, a panic included.
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            TAKEN.with(|t| t.borrow_mut().take());
        }
    }
    TAKEN.with(|t| *t.borrow_mut() = Some(None));
    let stop = Stop;
    let r = f();
    let bytes = TAKEN.with(|t| t.borrow_mut().take().flatten());
    drop(stop);
    (r, bytes)
}

/// A bytes field's value: `{"binary": <len>}` with the bytes moved to this thread's taker when it
/// has none yet, else the bytes as base64 (the text moves into the value; `json!` would copy it).
fn bytes_field(bytes: Cow<'_, [u8]>) -> Value {
    TAKEN.with(|t| match &mut *t.borrow_mut() {
        Some(slot @ None) => {
            let len = bytes.len();
            *slot = Some(bytes.into_owned());
            json!({"binary": len})
        }
        _ => Value::String(base64::engine::general_purpose::STANDARD.encode(&bytes)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tap_or_button_holds_the_platform_default_unless_hold_ms_says() {
        let ms = Duration::from_millis;
        let held = |p: Value| tap_samples(&p, ms(10)).expect("tap")[1].delay;
        assert_eq!(held(json!({"x": 1, "y": 2})), ms(10));
        assert_eq!(held(json!({"x": 1, "y": 2, "hold_ms": 60})), ms(60));
        assert_eq!(held(json!({"x": 1, "y": 2, "hold_ms": 0})), ms(0), "0 is a hold, not a missing one");
        assert_eq!(held(json!({"x": 1, "y": 2, "hold_ms": null})), ms(10), "the CLI sends null without --hold");
        assert_eq!(button(&json!({"button": "home"}), ms(80)), Ok((Button::Home, ms(80))));
        assert_eq!(button(&json!({"button": "home", "hold_ms": 5}), ms(80)), Ok((Button::Home, ms(5))));
    }

    #[test]
    fn bytes_go_beside_the_result_only_while_a_caller_takes_them() {
        let png = b"\x89PNG\r\n\x1a\n\0\xff".to_vec();
        let base64 = json!({"width": 2, "height": 1, "png": base64::engine::general_purpose::STANDARD.encode(&png)});
        let inline = || png_result(2, 1, &png, &json!({}));
        assert_eq!(inline(), Ok(base64.clone()), "nobody takes them: base64, as MCP gets");
        let (result, bytes) = taking_bytes(inline);
        assert_eq!(result, Ok(json!({"width": 2, "height": 1, "png": {"binary": png.len()}})));
        assert_eq!(bytes.as_ref(), Some(&png));
        // One attachment per reply: a second bytes field in the same call stays base64.
        let ((first, second), bytes) = taking_bytes(|| (inline(), inline()));
        assert_eq!((first.map(|r| r["png"].clone()), second), (Ok(json!({"binary": png.len()})), Ok(base64.clone())));
        assert_eq!(bytes.map(|b| b.len()), Some(png.len()));
        // A call that makes no bytes field takes none; another thread's result isn't taken; and
        // the thread stops taking once the call is over, even one that panicked.
        assert_eq!(taking_bytes(|| json!({"width": 2})), (json!({"width": 2}), None));
        let (elsewhere, bytes) = taking_bytes(|| std::thread::scope(|t| t.spawn(inline).join().expect("thread")));
        assert_eq!((elsewhere, bytes), (Ok(base64.clone()), None));
        assert!(std::panic::catch_unwind(|| taking_bytes(|| panic!("in the call"))).is_err());
        assert_eq!(inline(), Ok(base64));
        // With `path` the file holds them and nothing is inline.
        let path = std::env::temp_dir().join(format!("simee-png-result-{}.png", std::process::id()));
        let (saved, bytes) = taking_bytes(|| png_result(2, 1, &png, &json!({"path": path.to_str()})));
        assert_eq!((saved.map(|r| r["width"].clone()), bytes), (Ok(json!(2)), None));
        assert_eq!(std::fs::read(&path).ok(), Some(png));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn form_factor_inputs_parse() {
        assert_eq!(opt_orientation(&json!({})), Ok(None));
        assert_eq!(opt_orientation(&json!({"orientation": "landscape-left"})), Ok(Some(Orientation::LandscapeLeft)));
        assert!(opt_orientation(&json!({"orientation": "diagonal"})).expect_err("bad").contains("portrait, landscape-right"));
        use sim_core::duo::{Display, Screen};
        assert_eq!(display(&json!({})), Ok(Display::Active));
        assert_eq!(display(&json!({"display": "main"})), Ok(Display::Active));
        assert_eq!(display(&json!({"display": "carplay"})), Ok(Display::CarPlay));
        assert_eq!(display(&json!({"display": "inner"})), Ok(Display::Builtin(Screen::Inner)));
        assert!(display(&json!({"display": "hud"})).is_err());
        assert_eq!(crown_delta(&json!({"ticks": -3})), Ok(-3.0 * CROWN_UNITS_PER_TICK));
        assert_eq!(crown_delta(&json!({"delta": 12.5, "ticks": 1})), Ok(12.5));
        assert!(crown_delta(&json!({})).is_err());
    }

    #[test]
    fn ipad_input_parameters() {
        assert_eq!(opt_edge(&json!({})), Ok(None));
        assert_eq!(opt_edge(&json!({"edge": "bottom"})), Ok(Some(ScreenEdge::Bottom)));
        assert!(opt_edge(&json!({"edge": "bezel"})).expect_err("bad").contains("top, bottom, left, right"));

        let phases = |p: Value| pencil_samples(&p).map(|s| s.iter().map(|s| s.phase).collect::<Vec<_>>());
        assert_eq!(phases(json!({"x": 1, "y": 2})), Ok(vec![Phase::Down, Phase::Up]), "a tap by default");
        assert_eq!(phases(json!({"x": 1, "y": 2, "phase": "move"})), Ok(vec![Phase::Move]));
        let stroke = pencil_samples(&json!({"x": 0, "y": 0, "to_x": 100, "to_y": 50, "duration_ms": 170})).expect("stroke");
        assert_eq!(stroke.first().map(|s| s.phase), Some(Phase::Down));
        assert_eq!(stroke.last().map(|s| (s.phase, s.point)), Some((Phase::Up, Point::new(100.0, 50.0))));
        for phase in ["hover", "leave"] {
            let e = pencil_samples(&json!({"x": 1, "y": 2, "phase": phase})).expect_err(phase);
            assert!(e.contains("tap, down, move, up") && e.contains("only the Apple Pencil's position"), "{e}");
        }
        for (key, value) in [("pressure", json!(90)), ("button", json!(true)), ("eraser", json!(true))] {
            let e = pencil_samples(&json!({"x": 1, "y": 2, key: value})).expect_err(key);
            assert!(e.contains(&format!("no `{key}`")) && e.contains("simulator"), "{e}");
        }
        // What the CLI sends when none of them is asked for.
        let cli = json!({"x": 1, "y": 2, "phase": "tap", "pressure": null, "button": false, "eraser": false});
        assert_eq!(phases(cli), Ok(vec![Phase::Down, Phase::Up]));
        assert!(pencil_samples(&json!({"x": 1, "y": 2, "button": "yes"})).is_err());
        assert!(pencil_samples(&json!({"x": 1, "y": 2, "phase": "press"})).expect_err("unknown").contains("unknown phase"));
        assert_eq!(pencil_samples(&json!({"x": 1, "y": 2, "to_x": 3})), Err("a stroke needs both `to_x` and `to_y`".into()));

        let r = pointer_request(&json!({"x": 10, "y": 20})).expect("defaults");
        assert_eq!(
            r,
            PointerRequest {
                at: Point::new(10.0, 20.0),
                action: PointerAction::Move,
                button: PointerButton::Primary,
                hold: Duration::from_millis(80),
                drag_to: None
            }
        );
        let r = pointer_request(&json!({"x": 1, "y": 2, "action": "click", "button": "secondary"})).expect("click");
        assert_eq!((r.action, r.button), (PointerAction::Click, PointerButton::Secondary));
        let drag = pointer_request(&json!({"x": 1, "y": 2, "to_x": 30, "to_y": 40})).expect("drag");
        assert_eq!(drag.drag_to, Some(Point::new(30.0, 40.0)));
        assert!(pointer_request(&json!({"x": 1, "y": 2, "to_x": 30})).expect_err("half").contains("both `to_x` and `to_y`"));
        assert!(
            pointer_request(&json!({"x": 1, "y": 2, "to_x": 3, "to_y": 4, "action": "click"})).expect_err("both").contains("no `action`")
        );
        assert!(pointer_request(&json!({"x": 1, "y": 2, "action": "drag"})).expect_err("drag").contains("move, click, down, up"));
        assert!(pointer_request(&json!({"x": 1, "y": 2, "button": "middle"})).expect_err("middle").contains("primary, secondary"));
        assert!(pointer_request(&json!({"x": 1})).is_err());
    }

    #[test]
    fn stylus_parameters() {
        use stylus::PenPhase::*;
        let phases = |p: Value| stylus_samples(&p).map(|(s, d)| (s.iter().map(|s| s.phase).collect::<Vec<_>>(), d));
        assert_eq!(phases(json!({"x": 1, "y": 2})), Ok((vec![Down, Up, Leave], Duration::ZERO)), "a tap by default");
        assert_eq!(phases(json!({"x": 1, "y": 2, "phase": "hover"})), Ok((vec![Hover], Duration::ZERO)));
        let (stroke, d) = stylus_samples(&json!({"x": 0, "y": 0, "to_x": 100, "to_y": 0, "pressure": 90, "button": true})).expect("stroke");
        assert_eq!((stroke.len(), d), (13, Duration::from_millis(300)));
        assert!(stroke[..12].iter().all(|s| s.pressure == Some(90) && s.button) && stroke[12].phase == Leave);
        assert!(stylus_samples(&json!({"x": 1, "y": 2, "phase": "press"})).expect_err("phase").contains("tap, hover, down"));
        assert_eq!(stylus_samples(&json!({"x": 1, "y": 2, "pressure": 0})), Err("`pressure` must be 1-255".into()));
        assert_eq!(stylus_samples(&json!({"x": 1, "y": 2, "to_x": 3})), Err("a stroke needs both `to_x` and `to_y`".into()));
        assert_eq!(stylus_samples(&json!({"x": -1, "y": 2})), Err("`x` must be a whole number of pixels".into()));
    }

    #[test]
    fn vehicle_parameters_and_readings() {
        assert_eq!(vehicle_area(&json!({})), Ok(0));
        assert_eq!(vehicle_area(&json!({"area_id": 49})), Ok(49));
        assert_eq!(vehicle_area(&json!({"area_id": "0x31"})), Ok(49));
        assert!(vehicle_area(&json!({"area_id": true})).is_err());
        assert_eq!(vehicle_value(&json!({})), Ok(None));
        assert_eq!(vehicle_value(&json!({"value": "drive"})), Ok(Some("drive".into())));
        assert_eq!(vehicle_value(&json!({"value": 12.5})), Ok(Some("12.5".into())));
        assert_eq!(vehicle_value(&json!({"value": false})), Ok(Some("false".into())));
        assert_eq!(vehicle_value(&json!({"value": [2, 0]})), Ok(Some("2,0".into())));
        assert!(vehicle_value(&json!({"value": {"a": 1}})).is_err());
        let r = |id: u32, value: &str| vhal::Reading {
            property: vhal::label(id as i32),
            id: id as i32,
            area_id: 0,
            status: "available".into(),
            value: value.into(),
        };
        assert_eq!(reading_json(&r(0x1140_0400, "8"))["value"], json!(8));
        assert_eq!(reading_json(&r(0x1140_0400, "8"))["id"], json!("0x11400400"));
        assert_eq!(reading_json(&r(0x1160_0207, "12.5"))["value"], json!(12.5));
        assert_eq!(reading_json(&r(0x1120_0407, "true"))["value"], json!(true));
        assert_eq!(reading_json(&r(0x1141_0a01, "2,0"))["value"], json!([2, 0]));
        assert_eq!(reading_json(&r(0x1110_0101, "Toy Car"))["value"], json!("Toy Car"));
    }

    #[test]
    fn remote_swipes_cross_the_touch_surface() {
        let up = remote_swipe_samples(&json!({"direction": "up"})).expect("up");
        assert_eq!((up[0].phase, up.last().map(|s| s.phase)), (Phase::Down, Some(Phase::Up)));
        assert!(up[0].point.y > up.last().map_or(1.0, |s| s.point.y));
        assert!(up.iter().all(|s| (0.0..=1.0).contains(&s.point.x) && (0.0..=1.0).contains(&s.point.y)));
        let right = remote_swipe_samples(&json!({"direction": "right"})).expect("right");
        assert!(right[0].point.x < right.last().map_or(0.0, |s| s.point.x));
        assert!(remote_swipe_samples(&json!({"direction": "diagonal"})).is_err());
    }
}
