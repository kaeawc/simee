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
    let panel = s.showing_screen().then(|| match s.selected_info().map(|d| d.platform) {
        Some(Platform::Android) => android_panel(s, t),
        _ => crate::debug::panel(s, t),
    });
    // The panes' widths and visibility, and their drag handles.
    crate::panes::layout(s, t, sidebar(s, t, clock), stage(s, t, clock), panel)
}

/// A toolbar button: an icon (labelled for accessibility), with visible text only for the primary
/// action, which also gets the accent fill.
fn button(key: &str, glyph: &'static Icon, label: &str, action: Action, primary: bool, t: Theme) -> El {
    let b = crate::debug::icon_button(key, glyph, label, primary || key == "refresh-button", t, move |s: &mut State| {
        s.actions.push(action.clone())
    });
    if primary { b.fill(tok::accent(0, true)) } else { b }
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
    // Work for the whole window, and how many other devices have a job, beside Refresh.
    let window_work = s.window_busy.map(str::to_owned).or_else(|| crate::jobs::others_summary(&s.jobs, s.selected.as_deref()));
    let mut header = row().w_full().center_y().gap(6.0).child(text("simee").font(type_::TITLE).color(t.text).grow(1.0));
    if let Some(work) = window_work {
        header = header
            .child(crate::mark::mark(sim_core::mark::Motion::Working, clock, 14.0).key("window-busy-mark"))
            .child(text(work).key("window-busy").font(type_::MONO_CAPTION).color(t.text_secondary).truncate().min_w(0.0));
    }
    header = header.child(button("refresh-button", &icons::REFRESH, "Refresh", Action::Refresh, false, t));
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
        .on_input_change(|s: &mut State, _: &mut Tx| s.search_highlight = None)
        .on_input_key(crate::list::search_key);
    col()
        .w(240.0)
        .h_full()
        .padding(12.0)
        .gap(8.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        // Clear the window's title-bar buttons, which overlap the top-left corner.
        .child(el().h(22.0))
        .child(header)
        .child(search)
        .child(list.scroll(ScrollSpec { stick_bottom: false }).shrink().min_h(0.0))
}

/// One device's row: its state mark (booting while a boot from here runs), name and platform. A
/// click or Return selects it and boots it when it is shut down; ⌥ selects only ([`crate::list::activate`]).
/// Its keys name the device (`device-<udid>`), never its place: focus follows the device when a
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
        .key(format!("device-{}-mark", d.udid));
    let (udid, menu_udid) = (d.udid.clone(), d.udid.clone());
    let popped = s.popouts.is_open(&d.udid).then(|| crate::popout::row_mark(&d.udid, t));
    let mut badge = platform_badge(d, t).key(format!("device-{}-platform", d.udid));
    if selected {
        // The secondary colour is too faint on the tint.
        badge = badge.color(t.text);
    }
    // The bar's slot is there on every row, so names line up whichever is selected.
    let bar = el().key(format!("device-{}-bar", d.udid)).w(3.0).h(16.0).radius(1.5).fill(if selected {
        highlight.bar
    } else {
        Color::TRANSPARENT
    });
    let mut item = row()
        .key(format!("device-{}", d.udid))
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

fn stage(s: &State, t: Theme, clock: Option<f32>) -> El {
    let mut controls = row().key("controls").w_full().gap(8.0).center_y();
    // A boot from here says Booting before the service or the emulator does.
    let state = |d: &DeviceInfo| if s.jobs.booting(&d.udid) && d.state != DeviceState::Booted { "Booting" } else { d.state.as_str() };
    let title = s.selected_info().map_or_else(|| "Select a device".to_string(), |d| format!("{} · {} · {}", d.name, d.label(), state(d)));
    controls = controls.child(text(title).key("device-title").font(type_::SECTION).color(t.text).grow(1.0).truncate());
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
    if s.showing_screen()
        && popped.is_none()
        && let Some(d) = s.selected_info()
    {
        for c in crate::controls::for_device(d) {
            controls = controls.child(button(c.key, c.icon, c.label, c.action, false, t));
        }
        if booted {
            controls = controls.child(crate::popout::stage_button(&d.udid, t));
        }
    }
    // The 3D view's own: face on (⌘0) and flip (⌘F); orbit, pan and zoom are on the pointer.
    if crate::scene::active(s).is_some() {
        use crate::scene::Command;
        controls = controls
            .child(button("face-on-button", &icons::FACE_ON, "Face on (⌘0)", Action::View3d(Command::FaceOn), false, t))
            .child(button("flip-button", &icons::FLIP, "Flip (⌘F)", Action::View3d(Command::Flip), false, t));
    }
    let body = match popped {
        Some(udid) => crate::popout::placeholder(udid, t),
        None if s.showing_screen() => screen(s),
        None => placeholder(s, t, clock),
    };
    let mut stage = col().w_full().h_full().grow(1.0).min_w(0.0).padding(16.0).gap(12.0).child(controls).child(body);
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
        .child(el().h(22.0))
        .child(text("DEVICE").font(type_::SECTION).color(t.text_secondary))
        .child(facts)
        .child(text("LOG").font(type_::SECTION).color(t.text_secondary))
        .child(log.scroll(ScrollSpec { stick_bottom: true }).shrink().min_h(0.0))
}
