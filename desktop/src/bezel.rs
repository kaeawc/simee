//! The bezel: a band [`BAND`] points wide just outside the device's screen. A drag that starts
//! there comes in from that edge of the screen, as a drag from Simulator.app's bezel does. On iOS
//! it goes through the API's `touch` with `edge` (what `swipe --edge` sends), so home, the app
//! switcher, Notification Center and Control Center open from the window. Android has no edge
//! flag: its edge gestures (gesture navigation's back from the left or right, home from the
//! bottom, a watch's dismiss from the left) are ordinary touches that start at the screen's edge,
//! so there a bezel drag is a touch that starts on the edge pixel nearest the press. Devices
//! without edge gestures ([`crate::controls::bezel`]) have no bezel.

use std::cell::OnceCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use rune::{Drag, Phase as DragPhase};
use serde_json::{Value, json};
use sim_api::Session;
use sim_core::gesture::{Phase, ScreenEdge};

use crate::{Input, State, work};

/// How far outside the screen, in points, a press still lands on the bezel.
pub const BAND: f32 = 24.0;

/// How far, in points, a press on the bezel must move before it is a drag. A click that never
/// moves does nothing, as a click beside the screen did before the bezel existed (on Android the
/// edge touch would otherwise tap whatever is on the screen's edge pixel).
pub const DRAG_THRESHOLD: f32 = 6.0;

/// How a device's bezel drags reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A touch flagged with the edge it comes in from (the API's `touch` with `edge`).
    Ios,
    /// An ordinary touch that starts on the edge.
    Android,
}

/// The API, as the window calls it (a stand-in in tests).
pub type Call = fn(&mut Session, &str, &Value) -> Result<Value, String>;

/// What the window knows of the bezel: where the stage and the screen are, the edge of the drag
/// in progress, and the line to the API that iOS edge touches go out on.
pub struct Bezel {
    /// The stage around the screen, `[x, y, w, h]` in window points (drags arrive local to it).
    pub stage: [f32; 4],
    /// The screen, in window points.
    pub screen: [f32; 4],
    /// The edge the drag in progress came in from.
    pub edge: Option<ScreenEdge>,
    /// Where a press on the bezel landed (window points) while it has not moved
    /// [`DRAG_THRESHOLD`] yet: its touch-down is held until it does.
    pub press: Option<(f32, f32)>,
    call: Call,
    line: OnceCell<Option<mpsc::Sender<Sample>>>,
    /// Samples sent and not yet delivered.
    pending: Arc<AtomicUsize>,
    outbox: work::Outbox,
}

/// One touch sample on its way to the API, stamped when the window produced it.
struct Sample {
    params: Value,
    at: Instant,
    phase: Phase,
}

impl Default for Bezel {
    fn default() -> Self {
        Self::with_call(sim_api::call_method)
    }
}

impl Bezel {
    pub fn with_call(call: Call) -> Self {
        Self {
            stage: [0.0; 4],
            screen: [0.0; 4],
            edge: None,
            press: None,
            call,
            line: OnceCell::new(),
            pending: Arc::default(),
            outbox: work::Outbox::default(),
        }
    }

    /// Where failed edge touches are reported.
    pub fn set_outbox(&mut self, outbox: work::Outbox) {
        self.outbox = outbox;
    }

    /// Whether samples are still on their way: a touch sent directly now could overtake them.
    pub fn busy(&self) -> bool {
        self.pending.load(Ordering::Acquire) > 0
    }

    /// Sends one sample of an iOS touch through the API, in order, on its own thread (the first
    /// sample starts it): an edge touch with its `edge`, an on-screen touch without. The worker
    /// keeps the samples' spacing (a slow first call delays a gesture, it doesn't squeeze it into
    /// a burst); a failure goes to the status line.
    pub fn send(&self, udid: &str, at: (f32, f32), phase: Phase, edge: Option<ScreenEdge>) {
        let line = self.line.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Sample>();
            let (call, outbox, pending) = (self.call, self.outbox.clone(), self.pending.clone());
            let spawned = std::thread::Builder::new().name("simee-bezel".into()).spawn(move || {
                let mut session = Session::default();
                // The previous sample of the gesture in progress: when it was made and when it
                // went out. Each sample goes out no sooner than its gap after the previous one,
                // so a slow first call (the connect) or a late wake-up of this thread shifts the
                // rest of the gesture later instead of squeezing it into a burst (a fling).
                let mut prev: Option<(Instant, Instant)> = None;
                for sample in rx {
                    let now = Instant::now();
                    if let Some(due) = due_at(prev, sample.at, sample.phase).filter(|d| *d > now) {
                        std::thread::sleep(due - now);
                    }
                    if let Err(e) = call(&mut session, "touch", &sample.params) {
                        outbox.post(format!("edge swipe: {e}"));
                    }
                    // Stamped after the call: a slow call (the connect) is part of getting this
                    // sample out, and the next one keeps its gap after it.
                    let sent = Instant::now();
                    prev = (sample.phase != Phase::Up).then_some((sample.at, sent));
                    pending.fetch_sub(1, Ordering::Release);
                }
            });
            spawned.ok().map(|_| tx)
        });
        if let Some(line) = line {
            let params = touch_params(udid, at, phase, edge);
            self.pending.fetch_add(1, Ordering::AcqRel);
            if line.send(Sample { params, at: Instant::now(), phase }).is_err() {
                self.pending.fetch_sub(1, Ordering::Release);
            }
        }
    }
}

/// When a sample made at `at` may go out: its gap after the previous sample of the gesture
/// (`prev` is when that one was made and when it went out). `None` for a touch-down, which
/// starts a gesture, or when there is no previous sample.
fn due_at(prev: Option<(Instant, Instant)>, at: Instant, phase: Phase) -> Option<Instant> {
    let (made, sent) = prev.filter(|_| phase != Phase::Down)?;
    Some(sent + at.saturating_duration_since(made))
}

/// The edge of `screen` (`[x, y, w, h]`) whose bezel a press at `p` (same coordinates) lands
/// on: within [`BAND`] outside the screen. A press on the screen, or farther out, is not on the
/// bezel. Outside a corner the side it is farther beyond wins, and a tie goes to the top or
/// bottom (home and Control Center are the edge gestures people reach for).
pub fn hit(screen: [f32; 4], p: (f32, f32)) -> Option<ScreenEdge> {
    let [x, y, w, h] = screen;
    let (left, right, top, bottom) = (x - p.0, p.0 - (x + w), y - p.1, p.1 - (y + h));
    let out_x = left.max(right).max(0.0);
    let out_y = top.max(bottom).max(0.0);
    if (out_x == 0.0 && out_y == 0.0) || out_x > BAND || out_y > BAND {
        return None;
    }
    Some(if out_y >= out_x {
        if top > 0.0 { ScreenEdge::Top } else { ScreenEdge::Bottom }
    } else if left > 0.0 {
        ScreenEdge::Left
    } else {
        ScreenEdge::Right
    })
}

/// `p` (window points) in the device pixels of a screen of `size` drawn at `screen`, pulled onto
/// the screen: a press on the bezel lands on the screen's edge.
pub fn to_device(screen: [f32; 4], p: (f32, f32), size: (u32, u32)) -> (f32, f32) {
    let [x, y, w, h] = screen;
    let along = |v: f32, from: f32, len: f32, px: u32| ((v - from) / len.max(1.0) * px as f32).clamp(0.0, px.saturating_sub(1) as f32);
    (along(p.0, x, w, size.0), along(p.1, y, h, size.1))
}

/// The API's `touch` for one sample at `at` (device pixels), from `edge` if it is an edge touch.
pub fn touch_params(udid: &str, at: (f32, f32), phase: Phase, edge: Option<ScreenEdge>) -> Value {
    let phase = match phase {
        Phase::Down => "down",
        Phase::Move => "move",
        Phase::Up => "up",
    };
    let mut params = json!({"device": udid, "x": at.0.round(), "y": at.1.round(), "phase": phase});
    if let Some(edge) = edge {
        params["edge"] = edge.as_str().into();
    }
    params
}

/// The stage's drag handler: a drag that starts on the bezel queues edge touches (in device
/// pixels) until it ends, the first once the pointer has moved [`DRAG_THRESHOLD`] (a press that
/// never moves does nothing); any other press on the stage does nothing.
pub fn on_drag(s: &mut State, d: &Drag) {
    let [sx, sy, ..] = s.bezel.stage;
    let p = (sx + d.x, sy + d.y);
    let edge = match d.phase {
        DragPhase::Down => {
            s.bezel.edge = hit(s.bezel.screen, p);
            s.bezel.press = s.bezel.edge.map(|_| p);
            return;
        }
        DragPhase::Move => s.bezel.edge,
        DragPhase::Up => s.bezel.edge.take(),
    };
    let Some(edge) = edge else { return };
    let size = s.screen_size();
    if let Some(press) = s.bezel.press {
        if (p.0 - press.0).hypot(p.1 - press.1) < DRAG_THRESHOLD {
            if d.phase == DragPhase::Up {
                s.bezel.press = None;
            }
            return;
        }
        s.bezel.press = None;
        let (x, y) = to_device(s.bezel.screen, press, size);
        s.inputs.push(Input::Edge(x, y, Phase::Down, edge));
    }
    let (x, y) = to_device(s.bezel.screen, p, size);
    let phase = if d.phase == DragPhase::Up { Phase::Up } else { Phase::Move };
    s.inputs.push(Input::Edge(x, y, phase, edge));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    const SCREEN: [f32; 4] = [100.0, 50.0, 400.0, 600.0];

    #[test]
    fn presses_just_outside_the_screen_are_on_its_bezel() {
        assert_eq!(hit(SCREEN, (300.0, 660.0)), Some(ScreenEdge::Bottom));
        assert_eq!(hit(SCREEN, (480.0, 40.0)), Some(ScreenEdge::Top), "Control Center's corner");
        assert_eq!(hit(SCREEN, (90.0, 300.0)), Some(ScreenEdge::Left));
        assert_eq!(hit(SCREEN, (510.0, 300.0)), Some(ScreenEdge::Right));
        assert_eq!(hit(SCREEN, (300.0, 300.0)), None, "on the screen");
        assert_eq!(hit(SCREEN, (100.0, 50.0)), None, "the screen's own corner pixel");
        assert_eq!(hit(SCREEN, (300.0, 650.0 + BAND + 1.0)), None, "beyond the bezel");
        assert_eq!(hit(SCREEN, (300.0 + 300.0, 300.0)), None, "far to the side");
        // Outside a corner: the side it is farther beyond, the top or bottom on a tie.
        assert_eq!(hit(SCREEN, (95.0, 40.0)), Some(ScreenEdge::Top));
        assert_eq!(hit(SCREEN, (80.0, 45.0)), Some(ScreenEdge::Left));
        assert_eq!(hit(SCREEN, (90.0, 660.0)), Some(ScreenEdge::Bottom));
    }

    #[test]
    fn bezel_presses_land_on_the_screens_edge_in_device_pixels() {
        let size = (2064, 2752);
        assert_eq!(to_device(SCREEN, (300.0, 670.0), size), (1032.0, 2751.0));
        assert_eq!(to_device(SCREEN, (300.0, 20.0), size), (1032.0, 0.0));
        assert_eq!(to_device(SCREEN, (90.0, 350.0), size), (0.0, 1376.0));
        assert_eq!(to_device(SCREEN, (300.0, 350.0), size), (1032.0, 1376.0));
    }

    #[test]
    fn edge_touches_are_the_apis_touch_with_edge() {
        let p = touch_params("U1", (1032.4, 2751.0), Phase::Down, Some(ScreenEdge::Bottom));
        assert_eq!(p, json!({"device": "U1", "x": 1032.0, "y": 2751.0, "phase": "down", "edge": "bottom"}));
        assert_eq!(touch_params("U1", (5.0, 6.0), Phase::Move, Some(ScreenEdge::Top))["phase"], "move");
        assert_eq!(touch_params("U1", (5.0, 6.0), Phase::Up, Some(ScreenEdge::Left))["edge"], "left");
        // The method and its parameters are the API's own.
        let touch = sim_api::METHODS.iter().find(|m| m.0 == "touch").expect("touch method");
        assert!(touch_params("U1", (5.0, 6.0), Phase::Down, None).get("edge").is_none(), "an on-screen touch has no edge");
        for param in ["x", "y", "phase", "edge?"] {
            assert!(touch.1.contains(param), "{param} in {}", touch.1);
        }
    }

    static CALLS: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

    fn recording_call(_: &mut Session, method: &str, params: &Value) -> Result<Value, String> {
        CALLS.lock().map_err(|e| e.to_string())?.push((method.to_owned(), params.clone()));
        if params["phase"] == "up" { Err("device went away".into()) } else { Ok(Value::Null) }
    }

    #[test]
    fn samples_reach_the_api_in_order_and_failures_reach_the_status_line() {
        let mut bezel = Bezel::with_call(recording_call);
        let outbox = work::Outbox::default();
        bezel.set_outbox(outbox.clone());
        bezel.send("U1", (10.0, 2751.0), Phase::Down, Some(ScreenEdge::Bottom));
        bezel.send("U1", (10.0, 2000.0), Phase::Move, Some(ScreenEdge::Bottom));
        bezel.send("U1", (10.0, 1200.0), Phase::Up, Some(ScreenEdge::Bottom));
        let deadline = Instant::now() + Duration::from_secs(5);
        while CALLS.lock().expect("calls").len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let calls = CALLS.lock().expect("calls").clone();
        let phases: Vec<_> = calls.iter().map(|(m, p)| (m.as_str(), p["phase"].as_str().unwrap_or_default(), p["y"].as_f64())).collect();
        assert_eq!(phases, [("touch", "down", Some(2751.0)), ("touch", "move", Some(2000.0)), ("touch", "up", Some(1200.0))]);
        assert!(calls.iter().all(|(_, p)| p["edge"] == "bottom" && p["device"] == "U1"));
        let mut lines = Vec::new();
        while lines.is_empty() && Instant::now() < deadline {
            lines = outbox.take();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lines, ["edge swipe: device went away"]);
    }

    #[test]
    fn a_sample_is_due_its_made_gap_after_the_previous_one_went_out() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // Made 30 ms apart, the first delivered 150 ms late (the connect): the second is due 30 ms
        // after the first went out, not at once.
        assert_eq!(due_at(Some((t0, t0 + ms(150))), t0 + ms(30), Phase::Move), Some(t0 + ms(180)));
        // Made back to back stays back to back, whatever the clock did around it.
        assert_eq!(due_at(Some((t0, t0 + ms(150))), t0, Phase::Up), Some(t0 + ms(150)));
        assert_eq!(due_at(Some((t0, t0 + ms(5))), t0 + ms(30), Phase::Move), Some(t0 + ms(35)));
        // A touch-down starts a gesture; the first sample has nothing to follow.
        assert_eq!(due_at(Some((t0, t0 + ms(5))), t0 + ms(30), Phase::Down), None);
        assert_eq!(due_at(None, t0, Phase::Move), None);
    }

    static ORDER: Mutex<Vec<(String, Instant, Option<String>)>> = Mutex::new(Vec::new());

    fn slow_first_call(_: &mut Session, _: &str, params: &Value) -> Result<Value, String> {
        let mut seen = ORDER.lock().map_err(|e| e.to_string())?;
        if seen.is_empty() {
            std::thread::sleep(Duration::from_millis(150)); // the worker's connect
        }
        seen.push((params["phase"].as_str().unwrap_or_default().into(), Instant::now(), params["edge"].as_str().map(Into::into)));
        Ok(Value::Null)
    }

    /// A tap sent after a bezel swipe, while the swipe is still on its way, follows it through the
    /// same worker; and a slow first call delays the swipe without squeezing it into a burst.
    #[test]
    fn a_tap_after_a_bezel_swipe_follows_it_and_the_spacing_is_kept() {
        let bezel = Bezel::with_call(slow_first_call);
        assert!(!bezel.busy());
        let edge = Some(ScreenEdge::Bottom);
        // Bounds on the gaps the samples were made at: the sample is stamped inside `send`, so
        // its stamp lies between the test's clock before and after the call.
        let mut bounds = Vec::new();
        for (y, phase) in [(2751.0, Phase::Down), (2500.0, Phase::Move), (2200.0, Phase::Move), (1900.0, Phase::Move), (1700.0, Phase::Up)]
        {
            if phase != Phase::Down {
                std::thread::sleep(Duration::from_millis(30));
            }
            let before = Instant::now();
            bezel.send("U1", (10.0, y), phase, edge);
            bounds.push((before, Instant::now()));
        }
        assert!(bezel.busy(), "the first call is still connecting");
        bezel.send("U1", (500.0, 500.0), Phase::Down, None);
        bezel.send("U1", (500.0, 500.0), Phase::Up, None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while bezel.busy() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = ORDER.lock().expect("order").clone();
        let order: Vec<_> = seen.iter().map(|(p, _, e)| (p.as_str(), e.is_some())).collect();
        assert_eq!(order, [("down", true), ("move", true), ("move", true), ("move", true), ("up", true), ("down", false), ("up", false)]);
        // Delivered no closer than they were made, however loaded the runner: the least the made
        // gap can be is from the end of one `send` to the start of the next. (A loaded runner may
        // squeeze that to nothing, so this is a floor, never a fixed number of milliseconds.)
        for (i, pair) in seen[..5].windows(2).enumerate() {
            let gap = pair[1].1.duration_since(pair[0].1);
            let made_at_least = bounds[i + 1].0.saturating_duration_since(bounds[i].1);
            assert!(gap >= made_at_least, "delivered in a burst: {gap:?} < {made_at_least:?}");
        }
    }
}
