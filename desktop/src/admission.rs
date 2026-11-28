//! Admission before a boot (#275): the window asks the API's `admission_check` whether the host
//! can take the devices before it starts them. On admit it boots at once, with no UI. On hold or
//! backoff it shows a sheet with the numbers ("Booting pixel_9 would take memory used from 62%
//! to 91%"), every reason, and a choice: boot anyway, boot as many as the host admits now (when
//! that is some but not all), or cancel. It never blocks silently: a check that fails or can't
//! start boots anyway and says why in the status bar, and every sheet can be overridden.
//!
//! "Don't ask again this session" is remembered per platform and form factor, in memory only:
//! the next boot of that kind skips the check until the window quits. The sheet's "why" link
//! shows the [resource panel](crate::resources), whose gauges carry the host's verdict now.
//!
//! The check runs on a thread of its own (it samples the host and lists the running devices), so
//! the window never waits on it; headless scenes and tests run it inline on a stand-in API.

use std::collections::HashSet;
use std::sync::mpsc;

use rune::tok::{self, Theme, radius, type_};
use rune::{Color, Cx, El, col, el, row, text};
use serde_json::{Value, json};
use sim_api::Session;
use sim_core::device::DeviceInfo;
use sim_core::form_factor::FormFactor;
use sim_core::platform::Platform;

use crate::{Action, State};

/// The API, as the gate calls it (a stand-in in tests).
pub type Call = fn(&mut Session, &str, &Value) -> Result<Value, String>;

/// A kind of device: what "don't ask again" remembers.
pub type DeviceKind = (Platform, FormFactor);

/// Devices of one kind the window is about to boot.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub platform: Platform,
    pub form_factor: FormFactor,
    /// Each device's id (a UDID or an AVD name) and name, in boot order.
    pub devices: Vec<(String, String)>,
}

impl Request {
    /// One device; one with no known form factor is judged as a phone, as the API does.
    pub fn one(info: &DeviceInfo) -> Request {
        Request {
            platform: info.platform,
            form_factor: info.form_factor.unwrap_or(FormFactor::Phone),
            devices: vec![(info.udid.clone(), info.name.clone())],
        }
    }

    pub fn kind(&self) -> DeviceKind {
        (self.platform, self.form_factor)
    }

    /// `admission_check`'s parameters.
    pub fn params(&self) -> Value {
        json!({"platform": self.platform.as_str(), "form_factor": self.form_factor.as_str(), "count": self.devices.len()})
    }

    fn ids(&self, n: usize) -> Vec<String> {
        self.devices.iter().take(n).map(|(id, _)| id.clone()).collect()
    }

    /// What the sheet calls the devices: a single device by its name.
    fn what(&self, subject: Option<&str>) -> String {
        match (self.devices.as_slice(), subject) {
            ([(_, name)], _) => name.clone(),
            (_, Some(s)) if !s.is_empty() => s.to_owned(),
            (d, _) => format!("{} more {}", d.len(), kind_label(self.kind())),
        }
    }
}

/// "Android phone devices", "iOS tablet devices".
pub fn kind_label((platform, form_factor): DeviceKind) -> String {
    format!("{} {} devices", platform.label(), form_factor.as_str())
}

/// A choice on the sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Every device asked for, past the verdict.
    BootAnyway,
    /// The first `n` devices: as many as the host admits now.
    Boot(usize),
    Cancel,
}

impl Choice {
    pub fn key(self) -> &'static str {
        match self {
            Choice::BootAnyway => "admission-boot-anyway",
            Choice::Boot(_) => "admission-boot-some",
            Choice::Cancel => "admission-cancel",
        }
    }

    pub fn label(self) -> String {
        match self {
            Choice::BootAnyway => "Boot anyway".into(),
            Choice::Boot(n) => format!("Boot {n}"),
            Choice::Cancel => "Cancel".into(),
        }
    }
}

/// The warning shown when the host doesn't admit the devices.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    pub request: Request,
    /// `hold` or `backoff`.
    pub verdict: String,
    /// "Booting pixel_9 would take memory used from 62% to 91%".
    pub headline: String,
    /// The API's reasons, each with its numbers.
    pub reasons: Vec<String>,
    /// How many of the devices (in order) the host admits now.
    pub admissible: usize,
    /// The buttons, in the order they're drawn (the last is the default).
    pub choices: Vec<Choice>,
    /// "Don't ask again this session" is ticked.
    pub dont_ask: bool,
}

/// What the gate decided about a request.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Boot these devices now; `note` says why without a check (it failed, or was skipped).
    Proceed { devices: Vec<String>, note: Option<String> },
    /// Show the sheet.
    Ask(Sheet),
}

/// The buttons for `count` devices of which the host admits `admissible`: always boot anyway
/// and cancel, and boot N when the host takes some but not all.
pub fn choices(count: usize, admissible: usize) -> Vec<Choice> {
    let mut c = vec![Choice::BootAnyway];
    if admissible > 0 && admissible < count {
        c.push(Choice::Boot(admissible));
    }
    c.push(Choice::Cancel);
    c
}

fn pct(v: &Value) -> Option<(f64, f64)> {
    Some((v["now"].as_f64()?, v["after"].as_f64()?))
}

/// What the prediction does to the host, by the first kind of reason the API gave.
fn effect(a: &Value, verdict: &str) -> String {
    let p = &a["predicted"];
    let reasons: Vec<&str> = a["reasons"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let about = |what: &str| reasons.iter().any(|r| r.starts_with(what));
    if about("memory:") {
        if let Some((now, after)) = pct(&p["memory_used_pct"]) {
            return format!("take memory used from {now:.0}% to {after:.0}%");
        }
        if let Some((now, after)) = pct(&p["memory_available_bytes"]) {
            const GIB: f64 = (1u64 << 30) as f64;
            return format!("leave {:.1} GiB of memory available (from {:.1} GiB)", after / GIB, now / GIB);
        }
    }
    if about("load:")
        && let (Some((now, after)), Some(limit)) = (pct(&p["load"]), p["load"]["limit"].as_f64())
    {
        return format!("take the 1-minute load from {now:.2} to {after:.2} (limit {limit:.2})");
    }
    format!("go ahead on a host that already says {verdict}")
}

/// The gate's decision on `request` from `admission_check`'s answer: admit boots with no UI, hold
/// or backoff asks, and an answer that can't be read boots anyway, saying why.
pub fn decide(request: &Request, answer: Result<Value, String>) -> Decision {
    let all = request.ids(request.devices.len());
    let a = match answer {
        Ok(a) => a,
        Err(e) => return Decision::Proceed { devices: all, note: Some(format!("admission check failed ({e}); booting anyway")) },
    };
    let verdict = match a["verdict"].as_str() {
        Some("admit") => return Decision::Proceed { devices: all, note: None },
        Some(v @ ("hold" | "backoff")) => v.to_owned(),
        other => {
            let what = other.map_or_else(|| "no verdict".to_owned(), |v| format!("verdict `{v}`"));
            return Decision::Proceed { devices: all, note: Some(format!("admission check gave {what}; booting anyway")) };
        }
    };
    let count = request.devices.len();
    let admissible = a["predicted"]["admissible"].as_u64().map_or(0, |n| (n as usize).min(count));
    let reasons = a["reasons"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let headline = format!("Booting {} would {}", request.what(a["predicted"]["subject"].as_str()), effect(&a, &verdict));
    Decision::Ask(Sheet {
        request: request.clone(),
        verdict,
        headline,
        reasons,
        admissible,
        choices: choices(count, admissible),
        dont_ask: false,
    })
}

/// The window's admission state: the check in flight, the sheet, and the kinds not to ask about.
pub struct Gate {
    call: Call,
    /// Set by `setup` in a live window: without it the check runs inline.
    waker: Option<rune::Waker>,
    pending: Option<(Request, mpsc::Receiver<Result<Value, String>>)>,
    pub sheet: Option<Sheet>,
    /// Kinds whose sheet said "don't ask again": booted with no check until the window quits.
    pub skip: HashSet<DeviceKind>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::with_call(sim_api::call_method)
    }
}

impl Gate {
    pub fn with_call(call: Call) -> Self {
        Gate { call, waker: None, pending: None, sheet: None, skip: HashSet::new() }
    }

    pub fn connect(&mut self, waker: rune::Waker) {
        self.waker = Some(waker);
    }

    /// Whether a check or a sheet is waiting on the host or the user.
    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.sheet.is_some()
    }

    /// Asks about `request`: the decision now (a kind not to ask about, a check run inline, a
    /// thread that wouldn't start), or `None` while the check runs. A request while another
    /// waits is dropped: its sheet, once shown, covers the window.
    pub fn ask(&mut self, request: Request) -> Option<Decision> {
        if self.busy() {
            return None;
        }
        if self.skip.contains(&request.kind()) {
            let note = format!("booting without an admission check (don't ask again for {} this session)", kind_label(request.kind()));
            return Some(Decision::Proceed { devices: request.ids(request.devices.len()), note: Some(note) });
        }
        let (call, params) = (self.call, request.params());
        let Some(waker) = self.waker.clone() else {
            return Some(decide(&request, call(&mut Session::default(), "admission_check", &params)));
        };
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("simee-admission".into()).spawn(move || {
            let _ = tx.send(call(&mut Session::default(), "admission_check", &params));
            waker();
        });
        match spawned {
            Ok(_) => {
                self.pending = Some((request, rx));
                None
            }
            Err(e) => Some(decide(&request, Err(format!("no thread: {e}")))),
        }
    }

    /// The decision on the check in flight, once it has landed.
    pub fn take(&mut self) -> Option<Decision> {
        let (request, rx) = self.pending.as_ref()?;
        let answer = match rx.try_recv() {
            Ok(a) => a,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("the check stopped".into()),
        };
        let d = decide(request, answer);
        self.pending = None;
        Some(d)
    }
}

/// Carries out `d`: a sheet to show, or the devices to boot (returned) with its note in the
/// status bar.
pub fn apply(s: &mut State, d: Decision) -> Vec<String> {
    match d {
        Decision::Proceed { devices, note } => {
            if note.is_some() {
                s.status = note;
            }
            devices
        }
        Decision::Ask(sheet) => {
            s.admission.sheet = Some(sheet);
            Vec::new()
        }
    }
}

/// The boot button: the devices to boot now (none while the check runs or the sheet asks).
pub fn ask(s: &mut State, info: &DeviceInfo) -> Vec<String> {
    match s.admission.ask(Request::one(info)) {
        Some(d) => apply(s, d),
        None => Vec::new(),
    }
}

/// Every poll, before the window's actions: a landed check boots (queued as
/// [`Action::BootNow`]) or shows its sheet. Returns whether the window should redraw.
pub fn poll(s: &mut State) -> bool {
    let Some(d) = s.admission.take() else { return false };
    let devices = apply(s, d);
    if !devices.is_empty() {
        s.actions.push(Action::BootNow(devices));
    }
    true
}

/// A sheet button: boots what it says (remembering "don't ask again" for the kind) or cancels.
pub fn choose(s: &mut State, choice: Choice) {
    let Some(sheet) = s.admission.sheet.take() else { return };
    let n = match choice {
        Choice::BootAnyway => sheet.request.devices.len(),
        Choice::Boot(n) => n,
        Choice::Cancel => {
            s.status = Some(format!("Boot cancelled: the host says {}", sheet.verdict));
            return;
        }
    };
    if sheet.dont_ask {
        s.admission.skip.insert(sheet.request.kind());
    }
    s.actions.push(Action::BootNow(sheet.request.ids(n)));
}

/// The sheet's "why": the resource panel, with the inspector it lives in.
fn show_resources(s: &mut State) {
    let c = &mut s.prefs.current;
    c.resources.visible = true;
    c.inspector.visible = true;
}

// ----- view -------------------------------------------------------------------------------------

/// The window's root: the window with its screenshot previews, and the sheet over both.
pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let root = crate::shot_preview::view(s, cx);
    match &s.admission.sheet {
        Some(sheet) => root.child(scrim(sheet, tok::theme(true))),
        None => root,
    }
}

fn verdict_color(verdict: &str) -> Color {
    if verdict == "hold" { tok::accent(3, true) } else { tok::accent(2, true) }
}

fn sheet_button(choice: Choice, primary: bool, t: Theme) -> El {
    let mut b = row()
        .key(choice.key())
        .role("button")
        .label(choice.label())
        .h(28.0)
        .px(12.0)
        .center_y()
        .radius(radius::SM)
        .fill(if primary { tok::accent(0, true) } else { t.control_off })
        .on_press(move |s: &mut State| choose(s, choice))
        .child(text(choice.label()).font(type_::LABEL).color(t.text));
    if primary {
        b = b.autofocus();
    }
    b
}

/// The debug panel's switch, for "don't ask again".
fn switch(on: bool, t: Theme) -> El {
    let knob = el().size(14.0, 14.0).radius(7.0).fill(Color::WHITE);
    let mut track = row().w(30.0).h(18.0).padding(2.0).radius(9.0).fill(if on { tok::accent(0, true) } else { t.control_off });
    if on {
        track = track.child(el().grow(1.0));
    }
    track.child(knob)
}

/// The sheet, at the top of the window as a macOS sheet is, over a dimmed window that ignores
/// input; Escape cancels. The resource panel on the right stays in sight beside it.
fn scrim(sheet: &Sheet, t: Theme) -> El {
    let color = verdict_color(&sheet.verdict);
    let title = if sheet.verdict == "hold" { "The host is near its limit" } else { "The host is overloaded" };
    let mut reasons = col().key("admission-reasons").w_full().gap(4.0);
    for (i, r) in sheet.reasons.iter().enumerate() {
        reasons = reasons.child(row().w_full().gap(6.0).child(text("•").font(type_::BODY).color(t.text_secondary)).child(
            text(r.clone()).key(format!("admission-reason-{i}")).font(type_::BODY).color(t.text_secondary).wrap().grow(1.0).min_w(0.0),
        ));
    }
    let kind = kind_label(sheet.request.kind());
    let dont_ask = row()
        .key("admission-dont-ask")
        .role("switch")
        .label(format!("Don't ask again for {kind} this session"))
        .gap(8.0)
        .center_y()
        .on_press(|s: &mut State| {
            if let Some(sh) = s.admission.sheet.as_mut() {
                sh.dont_ask = !sh.dont_ask;
            }
        })
        .child(switch(sheet.dont_ask, t))
        .child(text(format!("Don't ask again for {kind} this session")).font(type_::LABEL).color(t.text));
    let why = text("Why? See the resource panel")
        .key("admission-why")
        .role("link")
        .font(type_::LABEL)
        .color(tok::accent(0, true))
        .on_press(show_resources);
    // Boot N is the default when the host takes some; otherwise Cancel.
    let default = sheet.choices.iter().copied().find(|c| matches!(c, Choice::Boot(_))).unwrap_or(Choice::Cancel);
    let mut buttons = row().w_full().gap(8.0).center_y().child(el().grow(1.0));
    for c in [Choice::Cancel].into_iter().chain(sheet.choices.iter().copied().filter(|c| *c != Choice::Cancel).rev()) {
        buttons = buttons.child(sheet_button(c, c == default, t));
    }
    let card = col()
        .key("admission-sheet")
        .role("alertdialog")
        .label(sheet.headline.clone())
        .w(500.0)
        .padding(18.0)
        .gap(12.0)
        .radius(radius::MD)
        .fill(t.card)
        .border(1.0, t.card_border)
        .shadow(8.0, 24.0, Color::BLACK.alpha(0.5))
        .modal(false, |s: &mut State| choose(s, Choice::Cancel))
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(rune::icon(&crate::icons::ALERT, 18.0).color(color))
                .child(text(title).font(type_::TITLE).color(t.text).grow(1.0))
                .child(text(sheet.verdict.clone()).key("admission-verdict").font(type_::LABEL).color(color)),
        )
        .child(text(sheet.headline.clone()).key("admission-headline").font(type_::BODY).color(t.text).wrap())
        .child(reasons)
        .child(why)
        .child(dont_ask)
        .child(buttons);
    col()
        .key("admission-scrim")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(Color::BLACK.alpha(0.45))
        .items_center()
        .pt(36.0)
        .child(card)
}

// ----- scenes -----------------------------------------------------------------------------------

/// A made-up `admission_check` answer on a 32 GiB host.
pub fn demo_answer(verdict: &str, devices: usize, admissible: usize, used: (f64, f64), reasons: &[&str]) -> Value {
    json!({
        "verdict": verdict,
        "reasons": reasons,
        "predicted": {
            "subject": format!("{devices} more Android phone device{}", if devices == 1 { "" } else { "s" }),
            "devices": devices,
            "admissible": admissible,
            "memory_used_pct": {"now": used.0, "after": used.1},
            "load": {"now": 3.1, "after": 3.1 + 2.5 * devices as f64, "limit": 15.0},
        }
    })
}

fn pixels(n: usize) -> Request {
    let devices =
        (0..n).map(|i| if i == 0 { "pixel_9".to_owned() } else { format!("pixel_9_{}", i + 1) }).map(|d| (d.clone(), d)).collect();
    Request { platform: Platform::Android, form_factor: FormFactor::Phone, devices }
}

fn show(s: &mut State, request: &Request, answer: Value) {
    if let Decision::Ask(sheet) = decide(request, Ok(answer)) {
        s.admission.sheet = Some(sheet);
    }
}

/// The headless `admission` scene: booting the shut-down Android tablet on a host that would hold.
pub fn scene(s: &mut State) {
    crate::resources::scene(s);
    s.selected = Some("pixel_tablet".into());
    let Some(tablet) = s.selected_info().cloned() else { return };
    let reason = "memory: 1 more Android tablet device would take memory used from 78% to 89% (3.8 GB each while booting), at or above the hold limit 85%";
    show(s, &Request::one(&tablet), demo_answer("hold", 1, 0, (78.0, 89.0), &[reason]));
}

/// The headless `admission-many` scene: three Android phones on a host that would back off,
/// which admits one of them now.
pub fn many_scene(s: &mut State) {
    crate::resources::scene(s);
    let reasons = [
        "memory: 3 more Android phone devices would take memory used from 62% to 97% (3.8 GB each while booting), at or above the backoff limit 95%",
        "load: 3 more Android phone devices would take the 1-minute load from 3.10 to 10.60 while booting, at or above the limit 15.00",
    ];
    show(s, &pixels(3), demo_answer("backoff", 3, 1, (62.0, 97.0), &reasons));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        /// `admit_api`'s calls on this test's thread (an inline check runs on the caller's).
        static CALLS: Cell<usize> = const { Cell::new(0) };
    }

    fn calls() -> usize {
        CALLS.with(Cell::get)
    }

    fn answer(verdict: &str, devices: usize, admissible: usize) -> Value {
        demo_answer(
            verdict,
            devices,
            admissible,
            (62.0, 91.0),
            &["memory: would take memory used from 62% to 91%, at or above the hold limit 85%"],
        )
    }

    fn admit_api(_: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
        assert_eq!(method, "admission_check");
        assert_eq!(p["platform"], "android");
        CALLS.with(|c| c.set(c.get() + 1));
        Ok(answer("admit", p["count"].as_u64().unwrap_or(1) as usize, 1))
    }

    fn hold_api(_: &mut Session, _: &str, p: &Value) -> Result<Value, String> {
        Ok(answer("hold", p["count"].as_u64().unwrap_or(1) as usize, 0))
    }

    fn failing_api(_: &mut Session, _: &str, _: &Value) -> Result<Value, String> {
        Err("no host sampler".into())
    }

    fn sheet(d: Decision) -> Sheet {
        match d {
            Decision::Ask(s) => s,
            other => panic!("expected a sheet, got {other:?}"),
        }
    }

    #[test]
    fn admit_boots_with_no_sheet() {
        let d = decide(&pixels(1), Ok(answer("admit", 1, 1)));
        assert_eq!(d, Decision::Proceed { devices: vec!["pixel_9".into()], note: None });
    }

    #[test]
    fn hold_and_backoff_ask_with_the_right_buttons() {
        let one = sheet(decide(&pixels(1), Ok(answer("hold", 1, 0))));
        assert_eq!(one.choices, [Choice::BootAnyway, Choice::Cancel], "one device: anyway or cancel");
        assert_eq!(one.verdict, "hold");
        assert_eq!(one.headline, "Booting pixel_9 would take memory used from 62% to 91%");
        assert_eq!(one.reasons.len(), 1);

        let three = sheet(decide(&pixels(3), Ok(answer("backoff", 3, 1))));
        assert_eq!(three.choices, [Choice::BootAnyway, Choice::Boot(1), Choice::Cancel]);
        assert_eq!(three.headline, "Booting 3 more Android phone devices would take memory used from 62% to 91%");
        assert_eq!(three.verdict, "backoff");

        // The host itself says hold (thermal, pressure): no device is admissible, no Boot N.
        let host = json!({"verdict": "hold", "reasons": ["thermal: serious"], "predicted": {"admissible": 0}});
        let h = sheet(decide(&pixels(2), Ok(host)));
        assert_eq!(h.choices, [Choice::BootAnyway, Choice::Cancel]);
        assert_eq!(h.headline, "Booting 2 more Android phone devices would go ahead on a host that already says hold");
    }

    #[test]
    fn boot_n_comes_from_admissible() {
        for (count, admissible, want) in [(3, 2, Some(2)), (5, 1, Some(1)), (3, 0, None), (3, 3, None), (2, 9, None)] {
            let s = sheet(decide(&pixels(count), Ok(answer("hold", count, admissible))));
            let n = s.choices.iter().find_map(|c| if let Choice::Boot(n) = c { Some(*n) } else { None });
            assert_eq!(n, want, "{count} devices, {admissible} admissible");
            assert_eq!(s.admissible, admissible.min(count), "never more than asked for");
        }
    }

    #[test]
    fn load_and_linux_memory_reasons_read_their_own_numbers() {
        let load = json!({"verdict": "hold", "reasons": ["load: …"], "predicted": {"load": {"now": 3.0, "after": 16.0, "limit": 15.0}}});
        assert_eq!(
            sheet(decide(&pixels(1), Ok(load))).headline,
            "Booting pixel_9 would take the 1-minute load from 3.00 to 16.00 (limit 15.00)"
        );
        let gib = (1u64 << 30) as f64;
        let linux = json!({"verdict": "backoff", "reasons": ["memory: …"], "predicted": {"memory_available_bytes": {"now": 10.0 * gib, "after": 2.5 * gib}}});
        assert_eq!(
            sheet(decide(&pixels(1), Ok(linux))).headline,
            "Booting pixel_9 would leave 2.5 GiB of memory available (from 10.0 GiB)"
        );
    }

    #[test]
    fn a_check_that_fails_never_blocks() {
        let d = decide(&pixels(1), Err("no host sampler".into()));
        let Decision::Proceed { devices, note } = d else { panic!("boots anyway") };
        assert_eq!(devices, ["pixel_9"]);
        assert!(note.is_some_and(|n| n.contains("no host sampler") && n.contains("booting anyway")));
        let odd = decide(&pixels(1), Ok(json!({"verdict": "maybe"})));
        assert!(matches!(odd, Decision::Proceed { note: Some(_), .. }), "an unknown verdict boots too");
        assert!(matches!(Gate::with_call(failing_api).ask(pixels(1)), Some(Decision::Proceed { .. })));
    }

    #[test]
    fn the_gate_asks_the_api_with_the_kind_and_count() {
        let mut g = Gate::with_call(admit_api);
        assert_eq!(pixels(3).params(), json!({"platform": "android", "form_factor": "phone", "count": 3}));
        assert!(matches!(g.ask(pixels(1)), Some(Decision::Proceed { note: None, .. })));
        assert_eq!(calls(), 1);
        assert!(!g.busy(), "admit leaves nothing waiting");
    }

    #[test]
    fn a_threaded_check_lands_on_the_next_take() {
        let mut g = Gate::with_call(hold_api);
        let woken = std::sync::Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        g.connect(std::sync::Arc::new(move || {
            w.fetch_add(1, Ordering::Release);
        }));
        assert_eq!(g.ask(pixels(1)), None, "in flight");
        assert!(g.busy());
        assert_eq!(g.ask(pixels(2)), None, "a second request while one waits is dropped");
        // The thread publishes the result, then wakes the window: so a wake means the result has
        // landed, but the result can be taken before the wake arrives. Wait for the wake.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while woken.load(Ordering::Acquire) == 0 {
            assert!(std::time::Instant::now() < deadline, "the window is woken");
            std::thread::yield_now();
        }
        let d = g.take().expect("a wake follows the result, so it has landed");
        assert_eq!(sheet(d).request, pixels(1));
        assert!(!g.busy());
    }

    mod window {
        use super::super::*;
        use super::{admit_api, calls};
        use rune::{Env, Key, KeyEvent, Mods, Runtime};

        fn runtime(scene: fn(&mut State)) -> Runtime<State> {
            let mut s = crate::init();
            scene(&mut s);
            let mut rt = Runtime::new(s, view, (1240.0, 870.0), Env::new(true, 2.0));
            rt.frame(0.0);
            rt
        }

        fn press(rt: &mut Runtime<State>, key: &str) {
            let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            rt.frame(1.0 / 60.0);
        }

        #[test]
        fn no_sheet_no_scrim() {
            let rt = runtime(crate::demo);
            assert!(rt.find("admission-scrim").is_none());
        }

        #[test]
        fn the_sheet_shows_the_numbers_and_its_buttons() {
            let rt = runtime(scene);
            for key in ["admission-sheet", "admission-headline", "admission-reason-0", "admission-why", "admission-dont-ask"] {
                assert!(rt.find(key).is_some(), "{key}");
            }
            assert!(rt.find("admission-boot-anyway").is_some() && rt.find("admission-cancel").is_some());
            assert!(rt.find("admission-boot-some").is_none(), "one device: no Boot N");
            let many = runtime(many_scene);
            assert!(many.find("admission-boot-some").is_some());
            assert!(many.find("admission-reason-1").is_some());
        }

        #[test]
        fn boot_n_boots_the_first_n_and_remembers_dont_ask() {
            let mut rt = runtime(many_scene);
            assert!(rt.state.admission.sheet.as_ref().is_some_and(|s| !s.dont_ask), "unticked at first");
            press(&mut rt, "admission-dont-ask");
            assert!(rt.state.admission.sheet.as_ref().is_some_and(|s| s.dont_ask));
            press(&mut rt, "admission-boot-some");
            assert_eq!(rt.state.actions, [Action::BootNow(vec!["pixel_9".into()])]);
            assert!(rt.state.admission.sheet.is_none() && rt.find("admission-scrim").is_none());
            assert!(rt.state.admission.skip.contains(&(Platform::Android, FormFactor::Phone)));
            // The next Android phone boots with no check; another kind is still asked about.
            rt.state.admission = Gate { skip: rt.state.admission.skip.clone(), ..Gate::with_call(admit_api) };
            let before = calls();
            let phone = rt.state.devices.iter().find(|d| d.udid == "pixel_9").cloned().expect("pixel_9");
            assert_eq!(ask(&mut rt.state, &phone), ["pixel_9"]);
            assert_eq!(calls(), before, "skipped");
            assert!(rt.state.status.as_deref().is_some_and(|s| s.contains("don't ask again")));
            let tablet = rt.state.devices.iter().find(|d| d.udid == "pixel_tablet").cloned().expect("pixel_tablet");
            assert_eq!(ask(&mut rt.state, &tablet), ["pixel_tablet"]);
            assert_eq!(calls(), before + 1, "asked");
        }

        #[test]
        fn boot_anyway_boots_every_device_and_cancel_none() {
            let mut rt = runtime(many_scene);
            press(&mut rt, "admission-boot-anyway");
            assert_eq!(rt.state.actions, [Action::BootNow(vec!["pixel_9".into(), "pixel_9_2".into(), "pixel_9_3".into()])]);
            assert!(rt.state.admission.skip.is_empty(), "not ticked: asked again next time");

            let mut rt = runtime(scene);
            press(&mut rt, "admission-cancel");
            assert!(rt.state.actions.is_empty() && rt.state.admission.sheet.is_none());
            assert!(rt.state.status.as_deref().is_some_and(|s| s.contains("hold")));

            let mut rt = runtime(scene);
            rt.key(KeyEvent { key: Key::Escape, mods: Mods::default() });
            rt.frame(1.0 / 60.0);
            assert!(rt.state.admission.sheet.is_none() && rt.state.actions.is_empty(), "Escape cancels");
        }

        #[test]
        fn the_sheet_covers_the_window_until_answered() {
            let mut rt = runtime(scene);
            let scrim = rt.elements().into_iter().find(|e| e.key.as_deref() == Some("admission-scrim")).expect("scrim");
            assert_eq!(scrim.rect, [0.0, 0.0, 1240.0, 870.0], "it dims the whole window");
            assert!(scrim.fill.a > 0.0);
            let (x, y) = rt.find("refresh-button").expect("refresh");
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            rt.frame(1.0 / 60.0);
            assert!(rt.state.actions.is_empty(), "the window behind ignores input");
        }

        #[test]
        fn why_shows_the_resource_panel_beside_the_sheet() {
            let mut rt = runtime(scene);
            rt.state.prefs.current.resources.visible = false;
            rt.state.prefs.current.inspector.visible = false;
            rt.frame(1.0 / 60.0);
            press(&mut rt, "admission-why");
            assert!(rt.state.prefs.current.resources.visible && rt.state.prefs.current.inspector.visible);
            assert!(rt.find("resources-verdict").is_some(), "the host's verdict now is in sight");
            assert!(rt.state.admission.sheet.is_some(), "the sheet stays");
        }
    }
}
