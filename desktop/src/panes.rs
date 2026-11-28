//! The window's side panes: the device list on the left and the platform panel (the inspector)
//! on the right. Each has a drag handle on its inner edge (double-click puts the default width
//! back) and hides with a shortcut: ⌘⌥S the sidebar, ⌘⌥I the inspector. Their widths and
//! visibility, the selected device and tab, and the window's frame persist through
//! [`crate::settings`].
//!
//! rune-kit's `SplitView` would do this, but rune-kit isn't one of the desktop's dependencies,
//! so the handles are the small ones here.

use std::cell::RefCell;
use std::time::Duration;

use rune::tok::Theme;
use rune::{Cursor, Drag, El, Key, KeyEvent, Phase, Tx, el, row};

use crate::State;
use crate::debug::Tab;
use crate::settings::{Pane, Settings, Store, WindowFrame};

/// A pane's width range, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub default: f32,
    pub min: f32,
    pub max: f32,
}

/// Which pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Sidebar,
    Inspector,
}

impl Side {
    pub fn range(self) -> Range {
        match self {
            Side::Sidebar => Range { default: 240.0, min: 180.0, max: 420.0 },
            Side::Inspector => Range { default: 340.0, min: 280.0, max: 600.0 },
        }
    }

    fn pane(self, s: &Settings) -> Pane {
        match self {
            Side::Sidebar => s.sidebar,
            Side::Inspector => s.inspector,
        }
    }

    fn pane_mut(self, s: &mut Settings) -> &mut Pane {
        match self {
            Side::Sidebar => &mut s.sidebar,
            Side::Inspector => &mut s.inspector,
        }
    }

    fn other(self) -> Side {
        match self {
            Side::Sidebar => Side::Inspector,
            Side::Inspector => Side::Sidebar,
        }
    }

    fn handle_key(self) -> &'static str {
        match self {
            Side::Sidebar => "sidebar-handle",
            Side::Inspector => "inspector-handle",
        }
    }
}

/// The narrowest the stage between the panes gets from a drag: the toolbar still fits.
pub const STAGE_MIN: f32 = 420.0;

/// The drag handle's width; it straddles the pane's inner edge.
pub const HANDLE: f32 = 8.0;

/// The traffic lights' clearance at the top of whatever is leftmost.
const TITLE_BAR: f32 = 22.0;

/// `width` for `side`, in its range: a width that isn't a number (or 0, unset) is the default.
/// With the window's width known (`window_w` > 0), a wider pane leaves the stage at least
/// [`STAGE_MIN`] beside `other_w`, the other pane's width (0 when hidden), but never below the
/// pane's own minimum.
pub fn clamp(side: Side, width: f32, window_w: f32, other_w: f32) -> f32 {
    let r = side.range();
    let w = if width.is_finite() && width > 0.0 { width.clamp(r.min, r.max) } else { r.default };
    if window_w > 0.0 { w.min(window_w - STAGE_MIN - other_w).max(r.min) } else { w }
}

/// The width `side` draws at (whether or not it shows).
pub fn width(s: &Settings, side: Side) -> f32 {
    clamp(side, side.pane(s).width, 0.0, 0.0)
}

/// What `side` takes of the window: its width, or 0 when hidden.
fn taken(s: &Settings, side: Side) -> f32 {
    if side.pane(s).visible { width(s, side) } else { 0.0 }
}

/// How much wider (or, negative, narrower) the panes make the window than the default layout
/// `fit_window` budgets: hidden or resized panes change the fitted window by as much.
pub fn chrome_delta(s: &Settings, inspector: bool) -> f32 {
    let inspector = if inspector { taken(s, Side::Inspector) - Side::Inspector.range().default } else { 0.0 };
    taken(s, Side::Sidebar) - Side::Sidebar.range().default + inspector
}

/// Brings loaded settings into range: pane widths, an unknown tab name.
pub fn sanitize(s: &mut Settings) {
    for side in [Side::Sidebar, Side::Inspector] {
        let pane = side.pane_mut(s);
        pane.width = if pane.width.is_finite() && pane.width > 0.0 { clamp(side, pane.width, 0.0, 0.0) } else { 0.0 };
    }
    if s.last_tab.as_deref().is_some_and(|t| tab_named(t).is_none()) {
        s.last_tab = None;
    }
}

/// The window's own state for the panes, which isn't saved.
#[derive(Debug, Default)]
pub struct Panes {
    /// The window's width in points, as last laid out (0 before the first layout).
    pub window_w: f32,
    /// The window opened at its saved size for this device: its first fit keeps that size.
    pub keep_window: Option<String>,
    /// The window opened at a saved position: the first live poll checks it's on a display.
    pub check_position: Option<(i32, i32)>,
}

/// The debug panel's tab as the settings name it.
pub fn tab_name(tab: Tab) -> &'static str {
    match tab {
        Tab::Device => "device",
        Tab::Network => "network",
        Tab::Storage => "storage",
        Tab::Processes => "processes",
        Tab::Logs => "logs",
    }
}

pub fn tab_named(name: &str) -> Option<Tab> {
    Tab::ALL.into_iter().find(|t| tab_name(*t) == name)
}

/// Starts the window from `store`: the selected device (unless `SIMEE_DEVICE` chose one) and
/// tab, and where the window opened.
pub fn restore(s: &mut State, mut store: Store) {
    sanitize(&mut store.current);
    let c = &store.current;
    if s.selected.is_none() {
        s.selected = c.last_device.clone();
    }
    if let Some(tab) = c.last_tab.as_deref().and_then(tab_named) {
        s.debug.tab = tab;
    }
    if let Some(frame) = c.window
        && store.persistent()
    {
        s.panes.keep_window = s.selected.clone().filter(|sel| c.last_device.as_ref() == Some(sel));
        s.panes.check_position = Some((frame.x, frame.y));
    }
    s.prefs = store;
}

/// The window's layout: the sidebar, its handle, the stage, and the inspector with its handle,
/// each pane at its width or left out while hidden.
pub fn layout(s: &State, t: Theme, sidebar: El, stage: El, inspector: Option<El>) -> El {
    let c = &s.prefs.current;
    let inspector = crate::resources::inspector(s, t, inspector);
    let mut root = row().key("window").w_full().h_full().fill(t.bg).on_layout(|s: &mut State, r: [f32; 4]| s.panes.window_w = r[2]);
    if c.sidebar.visible {
        let w = width(c, Side::Sidebar);
        root = root.child(sidebar.w(w).min_w(w).child(handle(Side::Sidebar).pin(None, Some(0.0), Some(-HANDLE / 2.0), Some(0.0))));
    }
    // With the sidebar hidden the stage is leftmost, under the traffic lights.
    root = root.child(if c.sidebar.visible { stage } else { stage.pt(16.0 + TITLE_BAR) });
    if let Some(panel) = inspector
        && c.inspector.visible
    {
        let w = width(c, Side::Inspector);
        root = root.child(panel.w(w).min_w(w).child(handle(Side::Inspector).pin(Some(-HANDLE / 2.0), Some(0.0), None, Some(0.0))));
    }
    root
}

/// A pane's drag handle: a strip on its inner edge. Dragging sets the width (cancelling puts it
/// back); a double-click puts the default back.
fn handle(side: Side) -> El {
    let label = match side {
        Side::Sidebar => "Resize the device list",
        Side::Inspector => "Resize the panel",
    };
    el().key(side.handle_key()).role("separator").label(label).w(HANDLE).cursor(Cursor::ResizeEw).on_drag(
        move |s: &mut State, d: &mut Drag, tx: &mut Tx| {
            let c = &mut s.prefs.current;
            if d.phase == Phase::Down {
                d.data = width(c, side);
                if d.clicks == 2 {
                    side.pane_mut(c).width = 0.0;
                }
                return;
            }
            let travel = if side == Side::Sidebar { d.dx } else { -d.dx };
            let want = if d.cancelled() { d.data } else { d.data + travel };
            let other = taken(c, side.other());
            side.pane_mut(c).width = clamp(side, want, s.panes.window_w, other);
            tx.request_poll();
        },
    )
}

/// Shows or hides `side`.
pub fn toggle(s: &mut State, side: Side) {
    let pane = side.pane_mut(&mut s.prefs.current);
    pane.visible = !pane.visible;
}

/// ⌘⌥S shows or hides the sidebar, ⌘⌥I the inspector, ⌘⌥R the resource panel in it.
pub fn on_key(s: &mut State, k: &KeyEvent, tx: &mut Tx) -> bool {
    let side = match k.key {
        Key::Command('s') if k.mods.alt && !k.mods.shift => Side::Sidebar,
        Key::Command('i') if k.mods.alt && !k.mods.shift => Side::Inspector,
        Key::Command('r') if k.mods.alt && !k.mods.shift => {
            crate::resources::toggle(s);
            tx.request_poll();
            return true;
        }
        _ => return false,
    };
    toggle(s, side);
    tx.request_poll();
    true
}

/// Every poll: notes the selection and tab, writes the settings once they've settled, and
/// checks a restored window landed on a display. Returns whether the window should redraw.
pub fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if s.selected_info().is_some() && s.prefs.current.last_device != s.selected {
        s.prefs.current.last_device = s.selected.clone();
    }
    let tab = Some(tab_name(s.debug.tab));
    if s.prefs.current.last_tab.as_deref() != tab {
        s.prefs.current.last_tab = tab.map(str::to_owned);
    }
    if let Some(secs) = s.prefs.tick(t) {
        tx.wake_after(Duration::from_secs_f64(secs));
    }
    if let Some(w) = s.prefs.take_warning() {
        s.status = Some(w);
        changed = true;
    }
    if !s.offline
        && let Some((x, y)) = s.panes.check_position.take()
    {
        let displays = rune::displays();
        if !displays.is_empty() && !displays.iter().any(|d| in_title_bar((x, y), d)) {
            tx.center_window();
        }
    }
    changed
}

/// Whether a window whose frame's top-left is at `(x, y)` has its title bar on `d`, so it can
/// be dragged from there.
pub fn in_title_bar((x, y): (i32, i32), d: &rune::Display) -> bool {
    let (px, py) = (x.saturating_add(80), y.saturating_add(10));
    let (dx, dy) = d.position;
    px >= dx && py >= dy && i64::from(px) < i64::from(dx) + i64::from(d.size.0) && i64::from(py) < i64::from(dy) + i64::from(d.size.1)
}

thread_local! {
    /// The window, for its frame as it closes (on the UI thread, where `placement` is read).
    static WINDOW: RefCell<Option<rune::WindowHandle>> = const { RefCell::new(None) };
}

/// `App::on_window`: keeps the window's handle.
pub fn remember_window(w: &rune::WindowHandle) {
    WINDOW.with(|cell| *cell.borrow_mut() = Some(w.clone()));
}

/// The frame to save from where the window is: none while minimized or full screen, which
/// aren't frames to reopen at.
pub fn frame_of(p: &rune::WindowPlacement) -> Option<WindowFrame> {
    if p.minimized || p.full_screen || p.scale <= 0.0 {
        return None;
    }
    let points = |px: u32| (f64::from(px) / p.scale) as f32;
    Some(WindowFrame { x: p.position.0, y: p.position.1, width: points(p.content_size.0), height: points(p.content_size.1) })
}

/// `App::on_close` and `App::on_quit`: notes the window's frame and writes the settings now.
pub fn on_close(s: &mut State, _: &mut Tx) {
    let placement = WINDOW.with(|cell| cell.borrow().as_ref().and_then(rune::WindowHandle::placement));
    if let Some(frame) = placement.as_ref().and_then(frame_of) {
        s.prefs.current.window = Some(frame);
    }
    s.prefs.flush();
}

/// Opens the window where `store` says it was: its size (points) and its frame's position.
pub fn open_at<S>(spec: &mut rune::AppSpec<S>, store: &Store) -> Option<rune::WindowOptions> {
    let frame = store.current.window?;
    if !(frame.width.is_finite() && frame.height.is_finite() && frame.width > 0.0 && frame.height > 0.0) {
        return None;
    }
    let (min_w, min_h) = spec.min_size.unwrap_or((0.0, 0.0));
    spec.size = (frame.width.max(min_w), frame.height.max(min_h));
    Some(rune::WindowOptions { position: Some((frame.x, frame.y)), ..rune::WindowOptions::default() })
}

/// Headless scenes: the demo with its panes hidden or resized.
pub fn sidebar_hidden_scene(s: &mut State) {
    crate::demo(s);
    s.prefs.current.sidebar.visible = false;
}

pub fn inspector_hidden_scene(s: &mut State) {
    crate::demo(s);
    s.prefs.current.inspector.visible = false;
}

pub fn panes_hidden_scene(s: &mut State) {
    crate::demo(s);
    s.prefs.current.sidebar.visible = false;
    s.prefs.current.inspector.visible = false;
}

pub fn panes_wide_scene(s: &mut State) {
    crate::demo(s);
    s.prefs.current.sidebar.width = 340.0;
    s.prefs.current.inspector.width = 460.0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Mods, Runtime};

    fn runtime(scene: fn(&mut State)) -> Runtime<State> {
        let mut s = crate::init();
        scene(&mut s);
        let mut rt = Runtime::new(s, crate::view::view, (1240.0, 870.0), Env::new(true, 2.0));
        rt.poll = Some(crate::poll);
        rt.on_key = Some(crate::on_key);
        rt.frame(0.0);
        rt
    }

    fn target(rt: &Runtime<State>, key: &str) -> Option<[f32; 4]> {
        rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).map(|e| e.target)
    }

    fn settle(rt: &mut Runtime<State>) {
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
    }

    fn cmd_alt(c: char) -> KeyEvent {
        KeyEvent { key: Key::Command(c), mods: Mods { cmd: true, alt: true, ..Mods::default() } }
    }

    #[test]
    fn pane_widths_stay_in_range() {
        let r = Side::Sidebar.range();
        assert_eq!(clamp(Side::Sidebar, 100.0, 0.0, 0.0), r.min);
        assert_eq!(clamp(Side::Sidebar, 9_000.0, 0.0, 0.0), r.max);
        assert_eq!(clamp(Side::Sidebar, 300.0, 0.0, 0.0), 300.0);
        for odd in [0.0, -5.0, f32::NAN, f32::INFINITY] {
            assert_eq!(clamp(Side::Inspector, odd, 0.0, 0.0), Side::Inspector.range().default, "{odd}");
        }
        // The stage keeps STAGE_MIN beside the other pane...
        assert_eq!(clamp(Side::Inspector, 600.0, 1240.0, 240.0), 1240.0 - STAGE_MIN - 240.0);
        // ...but a pane never goes below its own minimum for it.
        assert_eq!(clamp(Side::Sidebar, 400.0, 1000.0, 600.0), r.min);
    }

    #[test]
    fn loaded_settings_are_brought_into_range() {
        let mut s = Settings::default();
        s.sidebar.width = 5_000.0;
        s.inspector.width = -3.0;
        s.last_tab = Some("bogus".into());
        sanitize(&mut s);
        assert_eq!(s.sidebar.width, Side::Sidebar.range().max);
        assert_eq!(s.inspector.width, 0.0, "the default");
        assert_eq!(s.last_tab, None);
        assert_eq!(width(&s, Side::Inspector), 340.0);
    }

    #[test]
    fn the_default_layout_is_the_one_before_panes() {
        let rt = runtime(crate::demo);
        assert_eq!(target(&rt, "device-list").map(|r| r[0] > 0.0), Some(true));
        let panel = target(&rt, "debug-panel").unwrap();
        assert_eq!(panel[2], 340.0);
        assert_eq!(panel[0] + panel[2], 1240.0);
        assert!(target(&rt, "sidebar-handle").is_some() && target(&rt, "inspector-handle").is_some());
        assert_eq!(chrome_delta(&rt.state.prefs.current, true), 0.0, "the fitted window is as before");
    }

    #[test]
    fn shortcuts_hide_and_show_the_panes() {
        let mut rt = runtime(crate::demo);
        rt.key(cmd_alt('s'));
        settle(&mut rt);
        assert!(target(&rt, "device-list").is_none(), "the sidebar is hidden");
        assert!(!rt.state.prefs.current.sidebar.visible);
        assert!(rt.state.actions.is_empty(), "⌘⌥S is not a screenshot");
        rt.key(cmd_alt('i'));
        settle(&mut rt);
        assert!(target(&rt, "debug-panel").is_none(), "the inspector is hidden");
        assert!(target(&rt, "device-screen").is_some(), "the screen still shows");
        assert_eq!(chrome_delta(&rt.state.prefs.current, true), -580.0, "a fitted window is narrower by both panes");
        rt.key(cmd_alt('s'));
        rt.key(cmd_alt('i'));
        settle(&mut rt);
        assert!(target(&rt, "device-list").is_some() && target(&rt, "debug-panel").is_some());
    }

    #[test]
    fn dragging_a_handle_resizes_its_pane() {
        let mut rt = runtime(crate::demo);
        let (x, y) = rt.find("sidebar-handle").unwrap();
        rt.pointer_down(x, y);
        rt.pointer_move(x + 30.0, y);
        rt.pointer_move(x + 60.0, y);
        rt.pointer_up(x + 60.0, y);
        settle(&mut rt);
        assert_eq!(rt.state.prefs.current.sidebar.width, 300.0);
        // The inspector's handle drags the other way, and stops where the stage would get too narrow.
        let (x, y) = rt.find("inspector-handle").unwrap();
        rt.pointer_down(x, y);
        rt.pointer_move(x - 300.0, y);
        rt.pointer_move(x - 900.0, y);
        rt.pointer_up(x - 900.0, y);
        settle(&mut rt);
        assert_eq!(rt.state.prefs.current.inspector.width, 1240.0 - STAGE_MIN - 300.0);
        assert_eq!(target(&rt, "debug-panel").map(|r| r[2]), Some(1240.0 - STAGE_MIN - 300.0));
    }

    #[test]
    fn the_selection_and_tab_come_back() {
        let mut store = Store::default();
        store.current.last_device = Some("A".into());
        store.current.last_tab = Some("logs".into());
        let mut s = crate::init();
        s.selected = None;
        restore(&mut s, store);
        assert_eq!(s.selected.as_deref(), Some("A"));
        assert_eq!(s.debug.tab, Tab::Logs);
        assert_eq!(s.panes.keep_window, None, "no saved frame: the window fits as usual");
        for tab in Tab::ALL {
            assert_eq!(tab_named(tab_name(tab)), Some(tab));
        }
    }

    #[test]
    fn a_saved_frame_opens_the_window_there() {
        let mut spec = rune::AppSpec::<State>::new("t", (1240.0, 870.0), crate::init, crate::view::view);
        spec.min_size = Some((1000.0, 560.0));
        let mut store = Store::default();
        assert!(open_at(&mut spec, &store).is_none());
        store.current.window = Some(WindowFrame { x: 10, y: 20, width: 700.0, height: 900.0 });
        let options = open_at(&mut spec, &store).unwrap();
        assert_eq!(options.position, Some((10, 20)));
        assert_eq!(spec.size, (1000.0, 900.0), "never under the minimum");
        let p = rune::WindowPlacement { position: (5, 6), content_size: (2400, 1600), scale: 2.0, ..Default::default() };
        assert_eq!(frame_of(&p), Some(WindowFrame { x: 5, y: 6, width: 1200.0, height: 800.0 }));
        assert_eq!(frame_of(&rune::WindowPlacement { full_screen: true, ..p }), None);
    }
}
