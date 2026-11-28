//! The window: a device picker on the left (grouped by platform), the live screen and the controls
//! its form factor has in the middle, and a platform panel on the right (iOS: the debug panel;
//! Android: the guest log, logcat and the console's critical lines).

use rune::tok::{self, Theme, radius, type_};
use rune::{Color, Cx, Drag, El, Icon, KeyEvent, Phase, PointerEvent, ScrollSpec, SurfaceFit, Tx, col, el, row, text};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::form_factor::Shape;
use sim_core::platform::Platform;
use sim_core::rotation::Rotation;

use crate::{Action, Input, State, icons};

/// The window is always drawn in the dark theme.
const DARK: bool = true;

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let t = tok::theme(DARK);
    // The mark's clock: none under the system's reduced motion, so every mark rests.
    let clock = (!cx.system_reduced_motion()).then_some(s.mark_t);
    // The panel is there whether or not a screen is (#560): a selection, a boot or a pop-out
    // never shows or hides it.
    let panel = match s.selected_info().map(|d| d.platform) {
        _ if !s.showing_screen() => empty_panel(s, t),
        Some(Platform::Android) => android_panel(s, t),
        _ => crate::debug::panel(s, t),
    };
    let tools = Toolbar::of(s);
    // The panes' widths and visibility, their drag handles, and the title bar with their toggles
    // beside the traffic lights and at the window's top-right.
    let root = crate::panes::layout(s, t, cx.titlebar_inset(), sidebar(s, t, clock), stage(s, &tools, t, clock), panel);
    match tools.menu(s, t) {
        Some(menu) => root.child(menu),
        None => root,
    }
}

/// A toolbar button: an icon (labelled for accessibility), with visible text only for the primary
/// action, which also gets the accent fill.
fn button(key: &str, glyph: &'static Icon, label: &str, action: Action, primary: bool, t: Theme) -> El {
    let b = crate::debug::icon_button(key, glyph, label, primary || key == "refresh-button", t, move |s: &mut State| {
        s.actions.push(action.clone())
    });
    if primary { b.fill(tok::accent(0, true)) } else { b }
}

/// The 3D view's toggle (#591): a cube.
pub static CUBE: Icon = Icon {
    name: "simee-cube",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M12 3 20 7.5v9L12 21l-8-4.5v-9L12 3Z"/><path d="M4 7.5 12 12l8-4.5M12 12v9"/></g>"#,
};

/// The toolbar's overflow menu: three dots.
pub static MORE: Icon = Icon {
    name: "simee-more",
    view: 24.0,
    body: r#"<g fill="currentColor"><circle cx="5.5" cy="12" r="1.7"/><circle cx="12" cy="12" r="1.7"/><circle cx="18.5" cy="12" r="1.7"/></g>"#,
};

/// What the 3D view's toggle says.
pub const VIEW3D_LABEL: &str = "3D view (⌘3)";

/// A toggle for the toolbar or the title bar: pressed (the accent fill, and `checked` for
/// assistive technology) while `on`. With a reason it can't be used (`why`), it's dimmed, takes
/// no press, and its tooltip is the reason; otherwise its tooltip is its label.
pub fn toggle_button(key: &str, glyph: &'static Icon, label: &str, on: bool, why: Option<&str>, t: Theme) -> El {
    let b = row()
        .key(key.to_owned())
        .role("button")
        .label(label.to_owned())
        .help(why.unwrap_or(label).to_owned())
        .checked(on)
        .h(28.0)
        .px(7.0)
        .center_y()
        .radius(radius::SM)
        .fill(if on { tok::accent(0, true) } else { t.control_off });
    match why {
        Some(_) => b.disabled(true).child(rune::icon(glyph, 15.0).color(t.text_secondary.alpha(0.5))),
        None => b.child(rune::icon(glyph, 15.0).color(t.text)),
    }
}

/// The 3D view's toggle for the device on the stage: pressed while the view shows, and dimmed with
/// the reason where it can't (no compositor, a device with no body). A press is ⌘3's
/// [`Action::ToggleView3d`], so the status line and the view agree whichever was used.
pub fn view3d_button(s: &State, t: Theme) -> El {
    let why = if !s.view3d.supported {
        Some(crate::scene::UNAVAILABLE)
    } else if crate::scene::body_for(s).is_none() {
        Some(crate::scene::NO_BODY.as_str())
    } else {
        None
    };
    let b = toggle_button("view3d-button", &CUBE, VIEW3D_LABEL, crate::scene::active(s).is_some(), why, t);
    if why.is_some() { b } else { b.on_press(|s: &mut State| s.actions.push(Action::ToggleView3d)) }
}

/// One of the stage toolbar's device buttons: on the toolbar while there's room, else in its
/// overflow menu ([`Toolbar`]).
#[derive(Clone)]
struct Tool {
    key: String,
    glyph: &'static Icon,
    label: String,
    action: Action,
}

/// A device button and the gap before it, in points: the icon (15) and its padding (7 each side),
/// and the toolbar's gap (8).
const TOOL_W: f32 = 15.0 + 14.0 + 8.0;

/// The device title keeps at least this much of the toolbar before buttons go to the overflow
/// menu.
const TITLE_MIN: f32 = 140.0;

/// The stage toolbar's device buttons (#588, #591): the device's own, pop out, and the 3D view's
/// face on and flip, as many as fit after the title, the rest in an overflow menu (…) at the
/// end. The 3D toggle never goes there: it stays beside the device's buttons. The panes'
/// toggles are in the title bar, so nothing here can push them off.
struct Toolbar {
    /// Whether the 3D toggle shows (a running device on the stage).
    view3d: bool,
    shown: Vec<Tool>,
    more: Vec<Tool>,
}

impl Toolbar {
    fn of(s: &State) -> Toolbar {
        let mut tools = Vec::new();
        let popped = s.selected.as_deref().is_some_and(|u| s.popouts.is_open(u));
        let device = s.selected_info().filter(|_| s.showing_screen() && !popped);
        if let Some(d) = device {
            for c in crate::controls::for_device(d) {
                tools.push(Tool { key: c.key.into(), glyph: c.icon, label: c.label.into(), action: c.action });
            }
            if d.state == DeviceState::Booted {
                let (glyph, label) = crate::popout::STAGE_BUTTON;
                tools.push(Tool { key: "popout-button".into(), glyph, label: label.into(), action: Action::PopOut(d.udid.clone()) });
            }
        }
        // The 3D view's own: face on (⌘0) and flip (⌘F); orbit, pan and zoom are on the pointer.
        if crate::scene::active(s).is_some() {
            use crate::scene::Command;
            tools.push(Tool {
                key: "face-on-button".into(),
                glyph: &icons::FACE_ON,
                label: "Face on (⌘0)".into(),
                action: Action::View3d(Command::FaceOn),
            });
            tools.push(Tool {
                key: "flip-button".into(),
                glyph: &icons::FLIP,
                label: "Flip (⌘F)".into(),
                action: Action::View3d(Command::Flip),
            });
        }
        let view3d = device.is_some();
        // What's left of the stage's toolbar for buttons, once the layout has said how wide the
        // window is: the title's minimum, the Boot button or the job, and the 3D toggle first.
        let room = toolbar_width(s).map(|w| w - TITLE_MIN - lead_width(s) - if view3d { TOOL_W } else { 0.0 });
        let fit = room.map_or(tools.len(), |r| (r / TOOL_W).floor().max(0.0) as usize);
        let more = if fit < tools.len() { tools.split_off(fit.saturating_sub(1)) } else { Vec::new() };
        Toolbar { view3d, shown: tools, more }
    }

    /// The toolbar's buttons after the title and the Boot button or job.
    fn buttons(&self, s: &State, t: Theme) -> Vec<El> {
        let mut out: Vec<El> = self.shown.iter().map(|c| button(&c.key, c.glyph, &c.label, c.action.clone(), false, t)).collect();
        if self.view3d {
            // Beside the device's buttons, before the 3D view's own.
            let at = self.shown.iter().position(|c| matches!(c.action, Action::View3d(_))).unwrap_or(out.len());
            out.insert(at, view3d_button(s, t));
        }
        if !self.more.is_empty() {
            let open = s.panes.toolbar_menu;
            let names: Vec<&str> = self.more.iter().map(|c| c.label.as_str()).collect();
            let label = format!("More: {}", names.join(", "));
            out.push(toggle_button("toolbar-more", &MORE, &label, open, None, t).on_press(|s: &mut State| {
                s.panes.toolbar_menu = !s.panes.toolbar_menu;
            }));
        }
        out
    }

    /// The overflow menu, while it's open: under its button, over the window. A pick, a press
    /// outside it or Escape closes it.
    fn menu(&self, s: &State, t: Theme) -> Option<El> {
        if !s.panes.toolbar_menu || self.more.is_empty() {
            return None;
        }
        let mut menu = col()
            .key("toolbar-menu")
            .role("menu")
            .label("More")
            .anchored("toolbar-more", rune::Side::Bottom, rune::Align::End, 4.0)
            .modal(true, |s: &mut State| s.panes.toolbar_menu = false)
            .min_w(180.0)
            .padding(4.0)
            .gap(2.0)
            .radius(radius::MD)
            .fill(t.card)
            .border(1.0, t.card_border)
            .shadow(6.0, 18.0, Color::BLACK.alpha(0.45));
        for c in &self.more {
            let action = c.action.clone();
            menu = menu.child(
                row()
                    .key(format!("{}-more", c.key))
                    .role("menuitem")
                    .label(c.label.clone())
                    .h(28.0)
                    .px(8.0)
                    .gap(8.0)
                    .center_y()
                    .radius(radius::SM)
                    .hover(t.hover)
                    .on_press(move |s: &mut State| {
                        s.panes.toolbar_menu = false;
                        s.actions.push(action.clone());
                    })
                    .child(rune::icon(c.glyph, 15.0).color(t.text))
                    .child(text(c.label.clone()).font(type_::LABEL).color(t.text)),
            );
        }
        Some(menu)
    }
}

/// The stage toolbar's width inside its padding, once the window has been laid out.
fn toolbar_width(s: &State) -> Option<f32> {
    use crate::panes::{Side, shown, shown_width};
    let w = s.panes.window_w;
    let pane = |side| if shown(s, side) { shown_width(s, side) } else { 0.0 };
    (w > 0.0).then(|| w - pane(Side::Sidebar) - pane(Side::Inspector) - 32.0)
}

/// What the Boot button or the selected device's job takes of the toolbar, with its gap.
fn lead_width(s: &State) -> f32 {
    let booted = s.selected_info().is_some_and(|d| d.state == DeviceState::Booted);
    match s.selected.as_deref().and_then(|u| s.jobs.label(u)) {
        // The mark and the label's characters, about 7 points each.
        Some(busy) => 20.0 + 8.0 + 7.0 * busy.chars().count() as f32 + 8.0,
        None if s.selected.is_some() && !booted => 80.0,
        None => 0.0,
    }
}

fn sidebar(s: &State, t: Theme, clock: Option<f32>) -> El {
    let mut list = col().key("device-list").gap(2.0);
    if s.devices.is_empty() {
        list = list.child(text("No devices").key("devices-empty").font(type_::BODY).color(t.text_secondary).padding(8.0));
    }
    let highlighted = crate::list::highlighted(s);
    for row in crate::list::rows(s) {
        list = list.child(match row {
            crate::list::Row::Section { key, label } => text(label).key(key).font(type_::TITLE).color(t.text).px(4.0).padding(4.0),
            crate::list::Row::Runtime(label) => text(label).font(type_::SECTION).color(t.text_secondary).px(8.0).padding(4.0),
            crate::list::Row::NoMatches => {
                text("No matches").key("devices-no-match").font(type_::BODY).color(t.text_secondary).padding(8.0)
            }
            crate::list::Row::Device(i) => device_row(s, i, highlighted == Some(i), t, clock),
        });
    }
    let header = row().w_full().center_y().gap(6.0).child(text("simee").font(type_::TITLE).color(t.text).grow(1.0)).child(button(
        "refresh-button",
        &icons::REFRESH,
        "Refresh",
        Action::Refresh,
        false,
        t,
    ));
    // Work for the whole window, and how many other devices have a job: on a line of its own
    // under the title, so "1 device booting" isn't cut short beside Refresh (#580).
    let window_work = s.window_busy.map(str::to_owned).or_else(|| crate::jobs::others_summary(&s.jobs, s.selected.as_deref()));
    let work = window_work.map(|work| {
        row()
            .key("window-busy-row")
            .w_full()
            .gap(6.0)
            .center_y()
            .child(crate::mark::mark(sim_core::mark::Motion::Working, clock, 14.0).key("window-busy-mark"))
            .child(text(work).key("window-busy").font(type_::MONO_CAPTION).color(t.text_secondary).truncate().min_w(0.0))
    });
    let search = rune::text_input(&s.search, type_::BODY)
        .key(crate::list::SEARCH_KEY)
        .label("Search devices")
        .single_line()
        .placeholder("Search devices (⌘K)", t.text_secondary)
        .color(t.text)
        .w_full()
        .px(8.0)
        .py(5.0)
        .radius(radius::SM)
        .fill(t.control_off)
        .on_edit(|s: &mut State| &mut s.search)
        .on_input_change(|s: &mut State, _: &mut Tx| crate::list::query_changed(s))
        .on_input_key(crate::list::search_key);
    col()
        .w(240.0)
        .h_full()
        .padding(12.0)
        .gap(8.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(header)
        .children(work)
        .child(search)
        .child(list.scroll(ScrollSpec { stick_bottom: false }).shrink().min_h(0.0))
}

/// One device's row: its state mark (booting while a boot from here runs), name and platform. A
/// click or Return selects it and boots it when it is shut down; ⌥ selects only ([`crate::list::activate`]).
/// Its keys name the device ([`row_key`]), never its place: focus follows the device when a
/// rescan or a boot reorders the list, so Return presses the row the user focused.
fn device_row(s: &State, i: usize, highlighted: bool, t: Theme, clock: Option<f32>) -> El {
    let d = &s.devices[i];
    let selected = s.selected.as_deref() == Some(d.udid.as_str());
    let highlight = selection(t, DARK);
    // The row's mark agrees with the stage: a job on the device turns it.
    let job = s.jobs.label(&d.udid);
    let motion = job.map_or_else(|| crate::mark::for_device(d.state), crate::mark::for_busy);
    let moving = job.is_some() || crate::mark::moves(d.state);
    let state_mark = crate::mark::toned(motion, clock.filter(|_| moving), Some(crate::mark::tone(d.platform)), 16.0)
        .key(row_key("device-mark", &d.udid));
    let (udid, menu_udid) = (d.udid.clone(), d.udid.clone());
    let popped = s.popouts.is_open(&d.udid).then(|| crate::popout::row_mark(&d.udid, t));
    let mut badge = platform_badge(d, t).key(row_key("device-platform", &d.udid));
    if selected {
        // The secondary colour is too faint on the tint.
        badge = badge.color(t.text);
    }
    // The bar's slot is there on every row, so names line up whichever is selected.
    let bar =
        el().key(row_key("device-bar", &d.udid)).w(3.0).h(16.0).radius(1.5).fill(if selected { highlight.bar } else { Color::TRANSPARENT });
    let mut item = row()
        .key(row_key("device-row", &d.udid))
        .role("button")
        .label(format!("{}, {}", d.name, d.label()))
        .focusable(true)
        .press_on_return()
        .focus_selected(selected)
        .w_full()
        .h(30.0)
        .center_y()
        .radius(radius::SM)
        .on_press_tx(move |s: &mut State, tx: &mut Tx| crate::list::activate(s, &udid, tx.mods().alt))
        .on_secondary_press(move |s: &mut State, press, tx: &mut Tx| crate::popout::row_menu(s, menu_udid.clone(), press, tx))
        .child(bar)
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .pl(5.0)
                .pr(8.0)
                .gap(8.0)
                .center_y()
                .child(state_mark)
                .child(text(d.name.clone()).font(type_::BODY).color(t.text).grow(1.0).truncate())
                .children(popped)
                .child(badge),
        );
    if selected {
        item = item.fill(highlight.fill);
    } else if highlighted {
        item = item.fill(t.hover);
    }
    item
}

/// The key of a device row's `part` (`device-row`, `device-mark`, `device-bar`,
/// `device-platform`, `device-popped`) for `udid`: `device-row:pixel_9`. The colon can't be in a
/// fixed key (`device-screen`) nor in a part, so no AVD name makes a key that's another's: an AVD
/// named `screen` is `device-row:screen`, and `x-mark`'s row isn't `x`'s mark (#571).
pub fn row_key(part: &str, udid: &str) -> String {
    format!("{part}:{udid}")
}

/// How the selected device's row is drawn.
pub struct Selection {
    /// The row: the sidebar tinted toward the accent, under the primary text colour.
    pub fill: Color,
    /// A bar at the row's leading edge in the accent, which stands out from the sidebar on its
    /// own (the tint alone would be too faint at 3:1 without washing out the text).
    pub bar: Color,
}

/// The selected row's colours for theme `t` (`dark` or light). The text on the tint and the bar
/// against the sidebar meet WCAG's 4.5:1 and 3:1 in both (a test checks).
pub fn selection(t: Theme, dark: bool) -> Selection {
    let accent = tok::accent(0, dark);
    Selection { fill: t.card.mix(accent, if dark { 0.24 } else { 0.14 }), bar: accent }
}

/// The device's sub-platform (`iPadOS`, `watchOS`, `Wear OS`), else its platform.
pub fn platform_badge(d: &DeviceInfo, t: Theme) -> El {
    let name = d.sub_platform.map_or_else(|| d.platform.label(), |s| s.label());
    text(name).font(type_::MONO_CAPTION).color(t.text_secondary)
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-5` → `iOS 26.5`;
/// `system-images;android-36;google_apis;arm64-v8a` → `Android 36 · google_apis`.
pub fn runtime_label(id: &str) -> String {
    if let Some(["system-images", platform, tag, _abi]) = id.contains(';').then(|| id.split(';').collect::<Vec<_>>()).as_deref() {
        return format!("Android {} · {tag}", platform.trim_start_matches("android-"));
    }
    let tail = id.rsplit('.').next().unwrap_or(id);
    match tail.split_once('-') {
        Some((platform, version)) => format!("{platform} {}", version.replace('-', ".")),
        None => tail.to_owned(),
    }
}

fn stage(s: &State, tools: &Toolbar, t: Theme, clock: Option<f32>) -> El {
    let mut controls = row().key("controls").w_full().gap(8.0).center_y();
    // A boot from here says Booting before the service or the emulator does.
    let state = |d: &DeviceInfo| if s.jobs.booting(&d.udid) && d.state != DeviceState::Booted { "Booting" } else { d.state.as_str() };
    let title = s.selected_info().map_or_else(|| "Select a device".to_string(), |d| format!("{} · {} · {}", d.name, d.label(), state(d)));
    controls = controls.child(text(title).key("device-title").font(type_::SECTION).color(t.text).grow(1.0).min_w(0.0).truncate());
    let booted = s.selected_info().is_some_and(|d| d.state == DeviceState::Booted);
    let popped = s.selected.as_deref().filter(|u| s.popouts.is_open(u));
    // The selected device's own job only (#558): another device's boot isn't this one's.
    let job = s.selected.as_deref().and_then(|u| s.jobs.label(u));
    if let Some(busy) = job {
        controls = controls
            .child(crate::mark::mark(crate::mark::for_busy(busy), clock, 20.0).key("busy-mark"))
            .child(text(busy).key("busy").font(type_::BODY).color(t.text_secondary));
    } else if let Some(udid) = s.selected.as_ref().filter(|_| !booted) {
        controls = controls.child(button("boot-button", &icons::PLAY, "Boot", Action::Boot(udid.clone()), true, t));
    }
    controls = controls.children(tools.buttons(s, t));
    let body = match popped {
        Some(udid) => crate::popout::placeholder(udid, t),
        None if s.showing_screen() => screen(s),
        None => placeholder(s, t, clock),
    };
    let mut stage = col().w_full().h_full().grow(1.0).min_w(0.0).padding(16.0).gap(12.0).child(controls).child(body);
    // A pane the window is too narrow for: why it's gone, and that it comes back.
    if let Some(note) = crate::panes::narrow_note(s) {
        stage = stage.child(text(note).key("narrow-note").font(type_::LABEL).color(t.text).truncate());
    }
    let footer = match (&s.status, &s.socket) {
        (Some(msg), _) => Some(msg.clone()),
        (None, Some(sock)) => Some(format!("API: {sock}")),
        (None, None) => None,
    };
    if let Some(msg) = footer {
        stage = stage.child(text(msg).key("status").font(type_::MONO_CAPTION).color(t.text_secondary).truncate());
    }
    stage
}

fn placeholder(s: &State, t: Theme, clock: Option<f32>) -> El {
    let booting = s.selected.as_deref().is_some_and(|u| s.jobs.booting(u));
    let msg = if booting { "Waiting for the device to finish booting…" } else { "Boot a device to see its screen" };
    let motion = if booting { sim_core::mark::Motion::Boot } else { sim_core::mark::Motion::Idle };
    col()
        .key("placeholder")
        .w_full()
        .grow(1.0)
        .center()
        .gap(18.0)
        .child(crate::mark::mark(motion, clock.filter(|_| booting), 96.0).key("placeholder-mark"))
        .child(text(msg).font(type_::BODY).color(t.text_secondary))
}

/// Keys on the focused screen (or the 3D slot over it): the keystroke or shortcut goes to the
/// device; what maps to nothing is left to the window.
fn device_key(s: &mut State, k: &KeyEvent, _: &mut Tx) -> bool {
    let action = crate::keys::map(k);
    let handled = action != crate::keys::KeyAction::Ignore;
    if handled {
        s.inputs.push(Input::Key(action));
    }
    handled
}

/// The live screen. Pointer positions are local to the element, which has exactly the device's
/// aspect ratio, so they map to device pixels by scaling alone.
fn screen(s: &State) -> El {
    let (w, h) = s.screen_size();
    let ratio = w as f32 / h.max(1) as f32;
    // Under the 3D view the flat screen is only the scene's texture: square, so its corners hold
    // no stage colour for the body's rounded screen to show.
    let scene = crate::scene::active(s).is_some();
    let corners = if scene { 0.0 } else { screen_radius(s.shape, 10_000.0) };
    let device = el().key("device-screen").aspect_fit(ratio).radius(corners);
    let turn = s.screen_rotation();
    let device = match (&s.live, &s.screen) {
        (Some(_), _) => device.native_surface(&s.native, SurfaceFit::Fill),
        (None, Some(sc)) if turn != Rotation::Deg0 => turned(device, &sc.pixels, turn, ratio, corners),
        (None, Some(sc)) => device.pixels_fit(&sc.pixels, SurfaceFit::Fill),
        (None, None) if turn != Rotation::Deg0 => turned(device, &s.pixels, turn, ratio, corners),
        (None, None) => device.pixels(&s.pixels),
    };
    let device = device
        .focusable(true)
        .on_drag(|s: &mut State, d: &mut Drag, _: &mut Tx| {
            let (w, h) = s.screen_size();
            let to_device = |x: f32, y: f32| (x / d.w.max(1.0) * w as f32, y / d.h.max(1.0) * h as f32);
            let (x, y) = to_device(d.x, d.y);
            s.inputs.push(match d.phase {
                Phase::Down => Input::Down(x, y),
                Phase::Move => Input::Move(x, y),
                Phase::Up if d.cancelled() => Input::Up(x, y),
                Phase::Up => Input::Up(x, y),
            });
        })
        // With the stylus on (Android tablets), the pointer over the screen is the pen hovering.
        .on_layout(|s: &mut State, r: [f32; 4]| {
            s.screen_view = (r[2], r[3]);
            s.bezel.screen = r;
        })
        .on_pointer_move_local(|s: &mut State, p: PointerEvent, _: &mut Tx| {
            if s.stylus && !p.pressing {
                let (w, h) = s.screen_size();
                let (vw, vh) = s.screen_view;
                s.inputs.push(Input::Hover(p.x / vw.max(1.0) * w as f32, p.y / vh.max(1.0) * h as f32));
            }
        })
        .on_hover_change(|s: &mut State, inside: bool, _: &mut Tx| {
            if s.stylus && !inside {
                s.inputs.push(Input::Leave);
            }
        })
        .on_key(device_key);
    let stage = col().w_full().grow(1.0).min_h(0.0).center();
    // A drag from just outside the screen comes in from its edge (`crate::bezel`).
    let stage = match s.selected_info().and_then(crate::controls::bezel) {
        Some(_) => stage
            .key("device-bezel")
            .padding(crate::bezel::BAND)
            .on_layout(|s: &mut State, r: [f32; 4]| s.bezel.stage = r)
            .on_drag(|s: &mut State, d: &mut Drag, _: &mut Tx| crate::bezel::on_drag(s, d)),
        None => stage,
    };
    let stage = stage.child(device);
    if scene {
        let inset = if s.selected_info().and_then(crate::controls::bezel).is_some() { crate::bezel::BAND } else { 0.0 };
        // The slot takes the pointer, so it takes focus too (a click on the 3D view must not
        // blur the screen) and the screen's keys.
        stage.child(crate::scene::slot(inset, ratio, &s.view3d.view).focusable(true).on_key(device_key))
    } else {
        stage
    }
}

/// `pixels` as a turned device's panel lies, drawn turned clockwise by `turn` to fill `device`,
/// whose box has the upright screen's shape (`ratio`, width over height): Rune turns it as it
/// draws (on the GPU), so no frame is turned on the CPU. The picture is sized as fractions of
/// the box (sideways, its width is the box's height and its height the box's width), centred,
/// so it fills the box from the first frame and follows it as the window resizes. It takes no
/// input: `device` keeps its upright box, so pointer positions in it are the upright screen's.
pub fn turned(device: El, pixels: &rune::Pixels, turn: Rotation, ratio: f32, corners: f32) -> El {
    let (w, h) = if turn.is_sideways() { (1.0 / ratio.max(f32::EPSILON), ratio) } else { (1.0, 1.0) };
    let picture = el()
        .key("device-pixels")
        .w_frac(w)
        .h_frac(h)
        .radius(corners)
        .pixels_fit(pixels, SurfaceFit::Fill)
        .rotate((turn.degrees() as f32).to_radians())
        // The device turned: the picture shows the new way at once, as stock's window does,
        // and follows the box as the window resizes rather than springing after it.
        .instant_changes()
        .enter_in_instant();
    device.center().child(picture)
}

/// Corner radius, in points, for a screen of `width` points with this outline: a round watch is
/// a circle; phones and watches with corners get the device-like curve.
pub fn screen_radius(shape: Shape, width: f32) -> f32 {
    match shape {
        Shape::Round => width / 2.0,
        Shape::Rounded => 36.0_f32.min(width / 8.0),
        Shape::Rectangle => 4.0,
    }
}

/// What an Android panel says when there's no log: emu keeps the kernel console quiet, and the
/// logcat stream exists only for devices booted from here.
const EMPTY_LOG: &str = "No log yet. Devices booted here stream logcat (info and up) to this panel. \
                         The kernel console only has critical messages: for the full kernel log, \
                         start emu with -show-kernel or EMU_CONSOLE=full.";

/// The panel while no device screen shows (none selected, one shut down or booting, one out in
/// its own window): what it will show, and the resource panel's toggle in its corner.
fn empty_panel(s: &State, t: Theme) -> El {
    let msg = match s.selected.as_deref() {
        Some(udid) if s.popouts.is_open(udid) => "This device's window has its screen; its panel shows here once it's back",
        Some(_) => "The device's details show here once it's running",
        None => "Select a device to see its details here",
    };
    col()
        .key("inspector-empty")
        .w(340.0)
        .h_full()
        .padding(12.0)
        .gap(10.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(text("DEVICE").font(type_::SECTION).color(t.text_secondary))
        .child(text(msg).key("inspector-empty-note").font(type_::BODY).color(t.text_secondary).wrap())
}

/// An Android device's panel: what it is, and its guest log (the console's critical lines, then
/// logcat).
fn android_panel(s: &State, t: Theme) -> El {
    let mut facts = col().gap(4.0);
    if let Some(d) = s.selected_info() {
        let (w, h) = s.screen_size();
        for (key, line) in
            [("android-kind", d.label()), ("android-runtime", runtime_label(&d.runtime_id)), ("android-screen", format!("{w}×{h} px"))]
        {
            facts = facts.child(text(line).key(key).font(type_::MONO_CAPTION).color(t.text_secondary).truncate());
        }
    }
    let mut log = col().key("log").gap(1.0);
    for (i, line) in s.console_log.iter().enumerate() {
        log = log.child(text(line.clone()).key(format!("log-{i}")).font(type_::MONO_CAPTION).color(t.text).truncate());
    }
    if s.console_log.is_empty() {
        log = log.child(text(EMPTY_LOG).key("log-empty").font(type_::BODY).color(t.text_secondary).wrap());
    }
    col()
        .key("android-panel")
        .w(340.0)
        .h_full()
        .padding(12.0)
        .gap(10.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(text("DEVICE").font(type_::SECTION).color(t.text_secondary))
        .child(facts)
        .child(text("LOG").font(type_::SECTION).color(t.text_secondary))
        .child(log.scroll(ScrollSpec { stick_bottom: true }).shrink().min_h(0.0))
}

// ----- scenes -----------------------------------------------------------------------------------

/// The headless `toolbar-crowded` scene (#588, #591): a running Android phone with the 3D view on,
/// so the toolbar has the device's buttons, pop out, the 3D toggle, face on and flip. At the
/// minimum window width (`--size 1000x560`) the ones that don't fit are in the overflow menu.
pub fn crowded_toolbar_scene(s: &mut State) {
    crate::android_scene(s);
    s.view3d.supported = true;
    s.view3d.on = true;
}

/// The headless `toolbar-empty` scene: no device selected; the title bar's toggles are there.
pub fn empty_toolbar_scene(s: &mut State) {
    crate::demo(s);
    s.selected = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Key, KeyEvent, Runtime};

    fn runtime(scene: fn(&mut State), size: (f32, f32)) -> Runtime<State> {
        let mut s = crate::init();
        scene(&mut s);
        let mut rt = Runtime::new(s, view, size, Env::new(true, 2.0));
        rt.poll = Some(crate::poll);
        rt.on_key = Some(crate::on_key);
        settle(&mut rt);
        rt
    }

    fn settle(rt: &mut Runtime<State>) {
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
    }

    fn info(rt: &Runtime<State>, key: &str) -> Option<rune::ElementInfo> {
        rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key))
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        settle(rt);
    }

    /// The stage toolbar's buttons, left to right.
    fn toolbar(rt: &Runtime<State>) -> Vec<rune::ElementInfo> {
        let all = rt.elements();
        let Some(controls) = all.iter().position(|e| e.key.as_deref() == Some("controls")) else { return Vec::new() };
        let mut out: Vec<rune::ElementInfo> = all.iter().filter(|e| e.parent == Some(controls) && e.role == "button").cloned().collect();
        out.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        out
    }

    /// #588, #591: at the minimum window width, with both panes shown, a crowded toolbar keeps
    /// the 3D toggle and puts what doesn't fit in its overflow menu; nothing runs past the stage
    /// or under the title. A pick there does what the button does and closes the menu.
    #[test]
    fn a_crowded_toolbar_at_minimum_width_overflows_into_a_menu() {
        let mut rt = runtime(crowded_toolbar_scene, (1000.0, 560.0));
        let stage_right = 1000.0 - crate::panes::shown_width(&rt.state, crate::panes::Side::Inspector) - 16.0;
        let buttons = toolbar(&rt);
        let keys: Vec<&str> = buttons.iter().filter_map(|e| e.key.as_deref()).collect();
        assert!(keys.contains(&"view3d-button"), "the 3D toggle stays: {keys:?}");
        assert_eq!(keys.last(), Some(&"toolbar-more"), "the overflow menu ends it: {keys:?}");
        for b in &buttons {
            assert!(b.rect[0] + b.rect[2] <= stage_right + 0.5, "{:?} runs past the stage: {:?}", b.key, b.rect);
        }
        let title = info(&rt, "device-title").expect("title");
        assert!(title.rect[0] + title.rect[2] <= buttons[0].rect[0], "the title ends before the buttons");
        assert!(title.rect[2] >= TITLE_MIN - 8.0 - 0.5, "the title keeps its room: {:?}", title.rect);
        // The menu lists the rest, and a pick does what the button would.
        press(&mut rt, "toolbar-more");
        let items: Vec<String> = rt.elements().into_iter().filter(|e| e.role == "menuitem").filter_map(|e| e.key).collect();
        assert!(items.iter().any(|k| k == "flip-button-more"), "{items:?}");
        assert!(items.iter().all(|k| !keys.contains(&k.trim_end_matches("-more"))), "nothing is in both: {items:?}");
        rt.poll = None;
        press(&mut rt, "flip-button-more");
        assert!(rt.find("toolbar-menu").is_none(), "a pick closes the menu");
        assert_eq!(rt.state.actions, [Action::View3d(crate::scene::Command::Flip)]);
        // Escape closes it too.
        rt.state.actions.clear();
        press(&mut rt, "toolbar-more");
        assert!(rt.find("toolbar-menu").is_some());
        rt.key(KeyEvent::from(Key::Escape));
        settle(&mut rt);
        assert!(rt.find("toolbar-menu").is_none() && !rt.state.panes.toolbar_menu);
    }

    /// At the default size the same toolbar fits whole: no overflow menu.
    #[test]
    fn the_crowded_toolbar_fits_at_the_default_size() {
        let rt = runtime(crowded_toolbar_scene, (1240.0, 870.0));
        let keys: Vec<String> = toolbar(&rt).into_iter().filter_map(|e| e.key).collect();
        assert!(!keys.iter().any(|k| k == "toolbar-more"), "{keys:?}");
        for k in ["view3d-button", "face-on-button", "flip-button", "popout-button"] {
            assert!(keys.iter().any(|x| x == k), "{k}: {keys:?}");
        }
    }

    /// With no device the toolbar has no device buttons and no 3D toggle; the title says to
    /// select one.
    #[test]
    fn with_no_device_the_toolbar_has_only_its_title() {
        let rt = runtime(empty_toolbar_scene, (1000.0, 560.0));
        assert!(toolbar(&rt).is_empty(), "{:?}", toolbar(&rt).iter().map(|e| e.key.clone()).collect::<Vec<_>>());
        assert_eq!(info(&rt, "device-title").and_then(|e| e.text).as_deref(), Some("Select a device"));
        assert!(rt.find("sidebar-toggle").is_some() && rt.find("inspector-toggle").is_some());
    }

    /// #591: the cube toggles the 3D view the way ⌘3 does (the same status line and state),
    /// pressed while it's on, with "3D view (⌘3)" as its tooltip.
    #[test]
    fn the_3d_button_toggles_the_view_like_cmd_3() {
        let mut rt = runtime(crate::android_scene, (1240.0, 870.0));
        rt.update(|s, _| s.view3d.supported = true);
        settle(&mut rt);
        let b = info(&rt, "view3d-button").expect("the 3D toggle");
        assert_eq!(b.help.as_deref(), Some(VIEW3D_LABEL));
        assert_ne!(b.fill, tok::accent(0, true), "off: not pressed");
        press(&mut rt, "view3d-button");
        assert!(rt.state.view3d.on && rt.find("scene-3d").is_some());
        assert_eq!(rt.state.status.as_deref(), Some(crate::scene::ON), "as ⌘3 says");
        assert_eq!(info(&rt, "view3d-button").map(|e| e.fill), Some(tok::accent(0, true)), "pressed while on");
        assert!(rt.find("face-on-button").is_some() && rt.find("flip-button").is_some());
        rt.key(KeyEvent { key: Key::Command('3'), mods: rune::Mods { cmd: true, ..rune::Mods::default() } });
        settle(&mut rt);
        assert!(!rt.state.view3d.on, "⌘3 turns off what the button turned on");
        assert_ne!(info(&rt, "view3d-button").map(|e| e.fill), Some(tok::accent(0, true)));
    }

    /// #591: with no body for the device, or no compositor, the cube is there but dimmed, says
    /// why in its tooltip, and a press does nothing.
    #[test]
    fn the_3d_button_is_disabled_with_the_reason_when_there_is_no_body() {
        fn bodiless(s: &mut State) {
            crate::demo(s);
            // A device type no body matches, and no form factor to fall back on.
            let selected = s.selected.clone();
            s.devices.iter_mut().filter(|d| selected.as_deref() == Some(d.udid.as_str())).for_each(|d| {
                d.device_type_id = "com.example.NoSuchDevice".into();
                d.form_factor = None;
            });
            s.view3d.supported = true;
        }
        let mut rt = runtime(bodiless, (1240.0, 870.0));
        assert!(crate::scene::body_for(&rt.state).is_none(), "the scene's device has no body");
        let b = info(&rt, "view3d-button").expect("shown, so the view stays discoverable");
        assert_eq!(b.help.as_deref(), Some(crate::scene::NO_BODY.as_str()));
        assert_eq!(b.label.as_deref(), Some(VIEW3D_LABEL));
        rt.poll = None;
        press(&mut rt, "view3d-button");
        assert!(rt.state.actions.is_empty() && !rt.state.view3d.on, "a press does nothing");
        // No compositor (live Rust editing): the same, with that reason.
        let mut rt = runtime(crate::android_scene, (1240.0, 870.0));
        rt.update(|s, _| s.view3d.supported = false);
        settle(&mut rt);
        assert_eq!(info(&rt, "view3d-button").and_then(|e| e.help).as_deref(), Some(crate::scene::UNAVAILABLE));
    }

    /// #580: the count of other devices' boots has a line of its own under the title and
    /// Refresh, so "1 device booting" is whole at the default sidebar width, and at the minimum.
    #[test]
    fn the_booting_count_is_not_cut_short() {
        for width in [crate::panes::Side::Sidebar.range().default, crate::panes::Side::Sidebar.range().min] {
            let mut rt = runtime(crate::demo, (1240.0, 870.0));
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let work: crate::jobs::Work = Box::new(move || {
                let _ = rx.recv();
                Ok(())
            });
            rt.update(move |s, _| {
                s.prefs.current.sidebar.width = width;
                s.jobs.start("B2", crate::jobs::BOOTING, work, None).expect("a job");
            });
            settle(&mut rt);
            let count = info(&rt, "window-busy").expect("the count");
            assert_eq!(count.text.as_deref(), Some("1 device booting"));
            assert!(!count.text_cut, "whole at {width}: {:?}", count.rect);
            let refresh = info(&rt, "refresh-button").expect("refresh");
            assert!(count.rect[1] >= refresh.rect[1] + refresh.rect[3] - 0.5, "under the title and Refresh");
            drop(tx);
        }
    }
}
