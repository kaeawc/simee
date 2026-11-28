//! The window's side panes: the device list on the left and the platform panel (the inspector)
//! on the right. Each has a drag handle on its inner edge (double-click puts the default width
//! back) and shows and hides from its button in the title bar ([`title_bar`]: the list's beside
//! the traffic lights, the panel's at the window's top-right), the View menu (macOS) or a
//! shortcut: ⌘⌥S the sidebar, ⌘⌥I the inspector, ⌘⌥R the resource panel in it. Their widths and
//! visibility, the selected device and tab, and the window's frame persist through
//! [`crate::settings`].
//!
//! A pane shows or hides only on one of those explicit actions (#559, #560, #561), never on a
//! rescan, a selection change, a boot finishing or a pop-out: the inspector shows with no device
//! on screen too. The one exception is room. A window too narrow for the panes beside the stage's
//! [`STAGE_MIN`] squeezes them to their minimum widths, then collapses the inspector (and then
//! the list) with a note on the stage, and brings it back as soon as there's room. Every change
//! is logged with its [`Cause`] ([`Change`]): on stderr, and in [`Panes::changes`].
//!
//! rune-kit's `SplitView` would do this, but rune-kit isn't one of the desktop's dependencies,
//! so the handles are the small ones here.

use std::cell::RefCell;
use std::time::Duration;

use rune::tok::Theme;
use rune::{Cursor, Drag, El, Key, KeyEvent, Phase, Tx, col, el, row};

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

/// The title bar's height: the band across the window's top that the macOS traffic lights sit in
/// (AppKit's titlebar is 28 points), with the panes' toggles ([`title_bar`]).
pub const TITLE_H: f32 = 28.0;

/// The panes' toggles' gap from the traffic lights and from the window's right edge.
pub const TOGGLE_INSET: f32 = 8.0;

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

/// What made a pane show or hide, or the panel change mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// ⌘⌥S, ⌘⌥I or ⌘⌥R.
    Key,
    /// A toolbar button, or the panel's gauge button.
    Button,
    /// The View menu.
    Menu,
    /// The settings file, at start.
    Restore,
    /// The window got too narrow for the pane.
    Narrow,
    /// The window has room for it again.
    Room,
    /// The admission sheet's "Why?".
    Admission,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Key => "key",
            Cause::Button => "button",
            Cause::Menu => "menu",
            Cause::Restore => "restore",
            Cause::Narrow => "narrow",
            Cause::Room => "room",
            Cause::Admission => "admission",
        }
    }
}

/// What changed: a pane, or the panel's mode (the resource panel in place of the platform one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Sidebar,
    Inspector,
    Resources,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Target::Sidebar => "device list",
            Target::Inspector => "panel",
            Target::Resources => "resource panel",
        }
    }
}

impl From<Side> for Target {
    fn from(side: Side) -> Target {
        match side {
            Side::Sidebar => Target::Sidebar,
            Side::Inspector => Target::Inspector,
        }
    }
}

/// One logged change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub target: Target,
    pub shown: bool,
    pub cause: Cause,
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let verb = match (self.target, self.shown) {
            (Target::Resources, true) => "on",
            (Target::Resources, false) => "off",
            (_, true) => "shown",
            (_, false) => "hidden",
        };
        write!(f, "{} {verb} ({})", self.target.name(), self.cause.as_str())
    }
}

/// Changes [`Panes::changes`] keeps, newest last.
const KEPT_CHANGES: usize = 64;

/// Which panes the window is too narrow for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collapsed {
    pub sidebar: bool,
    pub inspector: bool,
}

impl Collapsed {
    fn of(self, side: Side) -> bool {
        match side {
            Side::Sidebar => self.sidebar,
            Side::Inspector => self.inspector,
        }
    }
}

/// The window's own state for the panes, which isn't saved.
#[derive(Debug, Default)]
pub struct Panes {
    /// The window's width in points, as last laid out (0 before the first layout).
    pub window_w: f32,
    /// Chosen panes the window is too narrow for right now ([`fit`]): back once there's room.
    pub collapsed: Collapsed,
    /// The last changes, newest last; each is also on stderr.
    pub changes: Vec<Change>,
    /// What the View menu said when it was last titled: the list, the panel and the resource
    /// panel shown.
    pub menu: Option<(bool, bool, bool)>,
    /// The window opened at its saved size for this device: its first fit keeps that size.
    pub keep_window: Option<String>,
    /// The window opened at a saved position: the first live poll checks it's on a display.
    pub check_position: Option<(i32, i32)>,
    /// The stage toolbar's overflow menu is open (`crate::view`'s toolbar).
    pub toolbar_menu: bool,
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
    // What the saved settings change from the defaults, so the next "it hid on its own" can
    // tell a restored pane from one hidden this run.
    let restored = [
        (Target::Sidebar, c.sidebar.visible, true),
        (Target::Inspector, c.inspector.visible, true),
        (Target::Resources, c.resources.visible, false),
    ];
    for (target, shown, default) in restored {
        if shown != default {
            note(&mut s.panes, Change { target, shown, cause: Cause::Restore });
        }
    }
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

/// Which of the panes `c` chooses can't fit a window `window_w` wide (0, not laid out yet, fits
/// both) beside the stage's [`STAGE_MIN`], even at their minimum widths: the inspector goes
/// first, then the list.
pub fn collapse(c: &Settings, window_w: f32) -> Collapsed {
    if window_w <= 0.0 {
        return Collapsed::default();
    }
    let room = window_w - STAGE_MIN;
    let (s_min, i_min) = (Side::Sidebar.range().min, Side::Inspector.range().min);
    let sidebar_min = if c.sidebar.visible { s_min } else { 0.0 };
    Collapsed { sidebar: c.sidebar.visible && s_min > room, inspector: c.inspector.visible && sidebar_min + i_min > room }
}

/// Whether `side` is on screen: chosen, and with room for it.
pub fn shown(s: &State, side: Side) -> bool {
    side.pane(&s.prefs.current).visible && !s.panes.collapsed.of(side)
}

/// The width `side` draws at now: its own, narrower while the window is (never under its
/// minimum), so the stage keeps [`STAGE_MIN`]. The list keeps its width first; the inspector
/// gives way. Nothing saved changes: a wider window gives the panes their widths back.
pub fn shown_width(s: &State, side: Side) -> f32 {
    let c = &s.prefs.current;
    let w = s.panes.window_w;
    let inspector_min = if shown(s, Side::Inspector) { Side::Inspector.range().min } else { 0.0 };
    let sidebar = clamp(Side::Sidebar, c.sidebar.width, w, inspector_min);
    match side {
        Side::Sidebar => sidebar,
        Side::Inspector => clamp(Side::Inspector, c.inspector.width, w, if shown(s, Side::Sidebar) { sidebar } else { 0.0 }),
    }
}

/// Brings [`Panes::collapsed`] up to date with the window's width and the panes chosen, and
/// logs each chosen pane the window collapses for room or has room for again. Every poll and
/// every layout of the window.
pub fn fit(s: &mut State) {
    let (was, now) = (s.panes.collapsed, collapse(&s.prefs.current, s.panes.window_w));
    if now == was {
        return;
    }
    s.panes.collapsed = now;
    for side in [Side::Sidebar, Side::Inspector] {
        // A pane hidden by hand neither comes back with room nor is hidden again without it.
        if now.of(side) != was.of(side) && side.pane(&s.prefs.current).visible {
            let cause = if now.of(side) { Cause::Narrow } else { Cause::Room };
            note(&mut s.panes, Change { target: side.into(), shown: !now.of(side), cause });
        }
    }
}

/// What the stage says while the window is too narrow for a chosen pane.
pub fn narrow_note(s: &State) -> Option<&'static str> {
    match (s.panes.collapsed.sidebar, s.panes.collapsed.inspector) {
        (true, true) => Some("The window is too narrow for the device list and the panel: widen it to bring them back"),
        (true, false) => Some("The window is too narrow for the device list: widen it to bring it back"),
        (false, true) => Some("The window is too narrow for the panel: widen it to bring it back"),
        (false, false) => None,
    }
}

/// The window's layout: the title bar, and under it the sidebar, its handle, the stage, and the
/// inspector with its handle, each pane at its width or left out while hidden (or collapsed for
/// room). `leading` is what the traffic lights take of the title bar (`Cx::titlebar_inset`).
pub fn layout(s: &State, t: Theme, leading: f32, sidebar: El, stage: El, platform: El) -> El {
    let mut panes = row().key("panes").w_full().grow(1.0).min_h(0.0);
    if shown(s, Side::Sidebar) {
        let w = shown_width(s, Side::Sidebar);
        panes = panes.child(sidebar.w(w).min_w(w).child(handle(Side::Sidebar).pin(None, Some(0.0), Some(-HANDLE / 2.0), Some(0.0))));
    }
    panes = panes.child(stage);
    if shown(s, Side::Inspector) {
        let w = shown_width(s, Side::Inspector);
        let panel = crate::resources::inspector(s, t, platform);
        panes = panes.child(panel.w(w).min_w(w).child(handle(Side::Inspector).pin(Some(-HANDLE / 2.0), Some(0.0), None, Some(0.0))));
    }
    col()
        .key("window")
        .w_full()
        .h_full()
        .fill(t.bg)
        .on_layout(|s: &mut State, r: [f32; 4]| {
            s.panes.window_w = r[2];
            fit(s);
        })
        .child(title_bar(s, t, leading))
        .child(panes)
}

/// The title bar (#588): the list's toggle beside the traffic lights and the panel's at the
/// window's top-right, as a macOS app's unified toolbar has them. Neither moves with the panes or
/// the stage, and both are there whatever the stage shows (no device, one shut down or booting,
/// one out in its own window) and however crowded its toolbar is. The rest of the band moves the
/// window: macOS's own titlebar drag is off (`Chrome::drag_regions`), so the toggles take presses.
pub fn title_bar(s: &State, t: Theme, leading: f32) -> El {
    row()
        .key("title-bar")
        .w_full()
        .h(TITLE_H)
        .min_h(TITLE_H)
        .pl(leading.max(0.0) + TOGGLE_INSET)
        .pr(TOGGLE_INSET)
        .center_y()
        .fill(t.card)
        .window_drag()
        .child(pane_toggle(s, Side::Sidebar, t))
        .child(el().grow(1.0))
        .child(pane_toggle(s, Side::Inspector, t))
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
            if d.phase == Phase::Down {
                // From the width it draws at, which a narrow window may have squeezed.
                d.data = shown_width(s, side);
                if d.clicks == 2 {
                    side.pane_mut(&mut s.prefs.current).width = 0.0;
                }
                return;
            }
            let travel = if side == Side::Sidebar { d.dx } else { -d.dx };
            let want = if d.cancelled() { d.data } else { d.data + travel };
            let other = if shown(s, side.other()) { shown_width(s, side.other()) } else { 0.0 };
            side.pane_mut(&mut s.prefs.current).width = clamp(side, want, s.panes.window_w, other);
            tx.request_poll();
        },
    )
}

/// Logs `change`: on stderr, the app's log (not in tests), and in [`Panes::changes`].
pub fn note(p: &mut Panes, change: Change) {
    if !cfg!(test) {
        eprintln!("simee-desktop: panes: {change}");
    }
    p.changes.push(change);
    if p.changes.len() > KEPT_CHANGES {
        p.changes.remove(0);
    }
}

/// Shows or hides `target` for `cause`, and logs it when that changes anything. Returns whether
/// it did.
pub fn set(s: &mut State, target: Target, visible: bool, cause: Cause) -> bool {
    let c = &mut s.prefs.current;
    let field = match target {
        Target::Sidebar => &mut c.sidebar.visible,
        Target::Inspector => &mut c.inspector.visible,
        Target::Resources => &mut c.resources.visible,
    };
    if *field == visible {
        return false;
    }
    *field = visible;
    note(&mut s.panes, Change { target, shown: visible, cause });
    fit(s);
    true
}

/// Hides `side` when it's on screen, else shows it, for `cause`. A pane the window is too narrow
/// for stays chosen, and the status line says why it doesn't show.
pub fn toggle(s: &mut State, side: Side, cause: Cause) {
    if shown(s, side) {
        set(s, side.into(), false, cause);
    } else {
        show(s, side, cause);
    }
}

/// Shows `side` for `cause`; while the window is too narrow for it, the status line says so.
pub fn show(s: &mut State, side: Side, cause: Cause) {
    set(s, side.into(), true, cause);
    if s.panes.collapsed.of(side) {
        s.status = narrow_note(s).map(str::to_owned);
    }
}

/// ⌘⌥S shows or hides the sidebar, ⌘⌥I the inspector, ⌘⌥R the resource panel in it.
pub fn on_key(s: &mut State, k: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(item) = ViewItem::for_key(k) else { return false };
    item.perform(s, Cause::Key);
    tx.request_poll();
    true
}

/// The View menu's items, which the shortcuts and the toolbar's buttons also do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewItem {
    /// ⌘⌥S.
    Sidebar,
    /// ⌘⌥I.
    Inspector,
    /// ⌘⌥R.
    Resources,
}

impl ViewItem {
    pub const ALL: [ViewItem; 3] = [ViewItem::Sidebar, ViewItem::Inspector, ViewItem::Resources];

    /// The key of its ⌘⌥ chord.
    pub fn key(self) -> char {
        match self {
            ViewItem::Sidebar => 's',
            ViewItem::Inspector => 'i',
            ViewItem::Resources => 'r',
        }
    }

    pub fn for_key(k: &KeyEvent) -> Option<ViewItem> {
        let Key::Command(c) = k.key else { return None };
        if !k.mods.alt || k.mods.shift {
            return None;
        }
        ViewItem::ALL.into_iter().find(|a| a.key() == c)
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn id(self) -> &'static str {
        match self {
            ViewItem::Sidebar => "view-device-list",
            ViewItem::Inspector => "view-panel",
            ViewItem::Resources => "view-resources",
        }
    }

    /// Whether what it shows is on screen.
    pub fn on(self, s: &State) -> bool {
        match self {
            ViewItem::Sidebar => shown(s, Side::Sidebar),
            ViewItem::Inspector => shown(s, Side::Inspector),
            ViewItem::Resources => shown(s, Side::Inspector) && s.prefs.current.resources.visible,
        }
    }

    /// Its title as the window is now: Show or Hide for a pane; the resource panel's item has a
    /// checkmark instead.
    pub fn title(self, s: &State) -> &'static str {
        match (self, self.on(s)) {
            (ViewItem::Sidebar, true) => "Hide Device List",
            (ViewItem::Sidebar, false) => "Show Device List",
            (ViewItem::Inspector, true) => "Hide Panel",
            (ViewItem::Inspector, false) => "Show Panel",
            (ViewItem::Resources, _) => "Resources",
        }
    }

    pub fn perform(self, s: &mut State, cause: Cause) {
        match self {
            ViewItem::Sidebar => toggle(s, Side::Sidebar, cause),
            ViewItem::Inspector => toggle(s, Side::Inspector, cause),
            ViewItem::Resources => crate::resources::toggle(s, cause),
        }
    }

    /// Picked in the View menu (macOS only; tests pick items directly).
    #[cfg(any(target_os = "macos", test))]
    pub fn pick(self, s: &mut State, tx: &mut Tx) {
        self.perform(s, Cause::Menu);
        tx.request_poll();
    }
}

/// The View menu, titled for the window as it is (macOS's menu bar; Rune has none elsewhere).
/// Each item's shortcut goes to the window's keys first ([`rune::MenuItem::keys_first`]), so a
/// press is logged as a key and a click in the menu as the menu.
#[cfg(target_os = "macos")]
pub fn menu(s: &State) -> Vec<rune::MenuItem<State>> {
    let item = |a: ViewItem, action: fn(&mut State, &mut Tx)| {
        let shortcut = format!("cmd+alt+{}", a.key()).parse().expect("a valid shortcut");
        let checked = a == ViewItem::Resources && a.on(s);
        rune::MenuItem::new(a.title(s), action).id(a.id()).menu("View").key_equivalent(shortcut).keys_first().checked(checked)
    };
    vec![
        item(ViewItem::Sidebar, |s, tx| ViewItem::Sidebar.pick(s, tx)),
        item(ViewItem::Inspector, |s, tx| ViewItem::Inspector.pick(s, tx)),
        item(ViewItem::Resources, |s, tx| ViewItem::Resources.pick(s, tx)),
    ]
}

#[cfg(target_os = "macos")]
thread_local! {
    /// The main window's app, to retitle its View menu (on the UI thread, where `poll` runs).
    static APP: RefCell<Option<rune::AppHandle<State>>> = const { RefCell::new(None) };
}

/// Keeps the main window's app for its View menu.
#[cfg(target_os = "macos")]
pub fn remember_app(handle: rune::AppHandle<State>) {
    APP.with(|cell| *cell.borrow_mut() = Some(handle));
}

/// The title bar's button for `side` (macOS's sidebar.left and sidebar.right), pressed while
/// its pane is on screen; its tooltip says what a press does and the shortcut.
pub fn pane_toggle(s: &State, side: Side, t: Theme) -> El {
    let (key, glyph, item) = match side {
        Side::Sidebar => ("sidebar-toggle", &crate::icons::SIDEBAR_LEFT, ViewItem::Sidebar),
        Side::Inspector => ("inspector-toggle", &crate::icons::SIDEBAR_RIGHT, ViewItem::Inspector),
    };
    let label = format!("{} (⌘⌥{})", item.title(s), item.key().to_ascii_uppercase());
    crate::view::toggle_button(key, glyph, &label, shown(s, side), None, t)
        .h(24.0)
        .on_press(move |s: &mut State| toggle(s, side, Cause::Button))
}

/// Retitles the View menu when what it says changed.
fn sync_menu(s: &mut State) {
    let now = (ViewItem::Sidebar.on(s), ViewItem::Inspector.on(s), ViewItem::Resources.on(s));
    if s.panes.menu == Some(now) {
        return;
    }
    s.panes.menu = Some(now);
    #[cfg(target_os = "macos")]
    APP.with(|cell| {
        if let Some(app) = cell.borrow().as_ref()
            && let Err(e) = app.set_menu(menu(s))
        {
            eprintln!("simee-desktop: View menu: {e}");
        }
    });
}

/// Every poll: notes the selection and tab, writes the settings once they've settled, and
/// checks a restored window landed on a display. Returns whether the window should redraw.
pub fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    fit(s);
    sync_menu(s);
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

/// The toolbar's toggles: the list hidden by its button (the button stays at the stage's
/// leading edge to bring it back), and the panel hidden by its.
pub fn toggled_list_scene(s: &mut State) {
    crate::demo(s);
    toggle(s, Side::Sidebar, Cause::Button);
}

pub fn toggled_panel_scene(s: &mut State) {
    crate::demo(s);
    toggle(s, Side::Inspector, Cause::Button);
}

/// The panel with no device on screen (a shut-down device selected): still there, as it was.
pub fn panel_empty_scene(s: &mut State) {
    crate::demo(s);
    s.selected = Some("B2".into());
    s.offline_screen = false;
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
        let changes = &rt.state.prefs.current;
        assert!(changes.sidebar.visible && changes.inspector.visible);
        let logged: Vec<(Target, bool, Cause)> = rt.state.panes.changes.iter().map(|c| (c.target, c.shown, c.cause)).collect();
        assert_eq!(
            logged,
            [
                (Target::Sidebar, false, Cause::Key),
                (Target::Inspector, false, Cause::Key),
                (Target::Sidebar, true, Cause::Key),
                (Target::Inspector, true, Cause::Key)
            ]
        );
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        settle(rt);
    }

    fn label(rt: &Runtime<State>, key: &str) -> String {
        rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).and_then(|e| e.label).unwrap_or_default()
    }

    fn help(rt: &Runtime<State>, key: &str) -> String {
        rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).and_then(|e| e.help).unwrap_or_default()
    }

    /// Where the two toggles are: each one's target rect.
    fn toggles(rt: &Runtime<State>) -> ([f32; 4], [f32; 4]) {
        let at = |k| target(rt, k).unwrap_or_else(|| panic!("{k} is on screen"));
        (at("sidebar-toggle"), at("inspector-toggle"))
    }

    /// Both toggles in the title bar, at their fixed places in a window `w` wide: the list's at
    /// the leading end (past the traffic lights), the panel's `TOGGLE_INSET` from the right edge.
    fn assert_in_place(rt: &Runtime<State>, w: f32, what: &str) {
        let (left, right) = toggles(rt);
        for r in [left, right] {
            assert!(r[1] >= 0.0 && r[1] + r[3] <= TITLE_H + 0.5, "{what}: in the title bar: {r:?}");
        }
        assert!(left[0] >= TOGGLE_INSET - 0.5 && left[0] < 120.0, "{what}: beside the traffic lights: {left:?}");
        assert!((right[0] + right[2] - (w - TOGGLE_INSET)).abs() < 0.5, "{what}: at the top-right: {right:?}");
    }

    /// #559, #560, #588: the title bar's buttons hide and show each pane with the mouse alone,
    /// stay where they are while the panes come and go, and show which is open: pressed (the
    /// accent fill) while its pane shows, with the shortcut in the tooltip.
    #[test]
    fn the_title_bar_buttons_hide_and_show_the_panes() {
        let mut rt = runtime(crate::demo);
        assert_eq!(label(&rt, "sidebar-toggle"), "Hide Device List (⌘⌥S)");
        assert_eq!(help(&rt, "sidebar-toggle"), "Hide Device List (⌘⌥S)", "the tooltip names the shortcut");
        assert_eq!(label(&rt, "inspector-toggle"), "Hide Panel (⌘⌥I)");
        let pressed = |rt: &Runtime<State>, k: &str| {
            rt.elements().into_iter().find(|e| e.key.as_deref() == Some(k)).is_some_and(|e| e.fill == rune::tok::accent(0, true))
        };
        assert!(pressed(&rt, "sidebar-toggle") && pressed(&rt, "inspector-toggle"), "both panes open");
        let home = toggles(&rt);
        assert_in_place(&rt, 1240.0, "both shown");
        press(&mut rt, "sidebar-toggle");
        assert!(target(&rt, "device-list").is_none(), "the list is hidden");
        assert_eq!(label(&rt, "sidebar-toggle"), "Show Device List (⌘⌥S)");
        assert!(!pressed(&rt, "sidebar-toggle") && pressed(&rt, "inspector-toggle"));
        press(&mut rt, "inspector-toggle");
        assert!(target(&rt, "debug-panel").is_none());
        assert!(!pressed(&rt, "inspector-toggle"));
        assert_eq!(toggles(&rt), home, "neither moved with the panes");
        press(&mut rt, "sidebar-toggle");
        press(&mut rt, "inspector-toggle");
        assert!(target(&rt, "device-list").is_some() && target(&rt, "debug-panel").is_some());
        assert_eq!(toggles(&rt), home);
        assert!(rt.state.panes.changes.iter().all(|c| c.cause == Cause::Button) && rt.state.panes.changes.len() == 4);
        assert!(rt.state.actions.is_empty(), "the buttons queue nothing for the device");
    }

    /// #588: both toggles are there and in place whatever the stage shows (no device, one shut
    /// down, one booting, one in its own window, a running one with a crowded toolbar), at the
    /// default size and the minimum window width, and each still works there.
    #[test]
    fn the_pane_toggles_are_always_there() {
        fn no_device(s: &mut State) {
            crate::demo(s);
            s.selected = None;
        }
        fn booting(s: &mut State) {
            panel_empty_scene(s);
            s.devices.iter_mut().filter(|d| d.udid == "B2").for_each(|d| d.state = sim_core::device::DeviceState::Booting);
        }
        type Scene = fn(&mut State);
        let scenes: [(&str, Scene); 7] = [
            ("demo", crate::demo),
            ("no device", no_device),
            ("shut down", panel_empty_scene),
            ("booting", booting),
            ("popped out", crate::popout::scene),
            ("crowded", crate::view::crowded_toolbar_scene),
            ("lists hidden", panes_hidden_scene),
        ];
        for (name, scene) in scenes {
            for w in [1240.0, 1000.0] {
                let mut rt = runtime(scene);
                rt.set_viewport((w, 560.0), 2.0);
                settle(&mut rt);
                assert_in_place(&rt, w, &format!("{name} at {w}"));
                let list = rt.state.prefs.current.sidebar.visible;
                press(&mut rt, "sidebar-toggle");
                assert_ne!(rt.state.prefs.current.sidebar.visible, list, "{name} at {w}: the list's toggle works");
                let panel = rt.state.prefs.current.inspector.visible;
                press(&mut rt, "inspector-toggle");
                assert_ne!(rt.state.prefs.current.inspector.visible, panel, "{name} at {w}: the panel's toggle works");
                assert_in_place(&rt, w, &format!("{name} at {w}, toggled"));
            }
        }
    }

    /// The View menu's items (macOS's menu bar) do what the buttons do, titled for what they'll
    /// do next, and logged as the menu.
    #[test]
    fn the_view_menu_hides_and_shows_the_panes() {
        let mut rt = runtime(crate::demo);
        let titles = |rt: &Runtime<State>| ViewItem::ALL.map(|a| a.title(&rt.state));
        assert_eq!(titles(&rt), ["Hide Device List", "Hide Panel", "Resources"]);
        assert!(!ViewItem::Resources.on(&rt.state));
        for item in ViewItem::ALL {
            rt.update(move |s, tx| item.pick(s, tx));
        }
        settle(&mut rt);
        assert!(target(&rt, "device-list").is_none(), "Hide Device List");
        // Hide Panel, then Resources: the resource panel, with its panel shown again.
        assert!(target(&rt, "resources-panel").is_some());
        assert_eq!(titles(&rt), ["Show Device List", "Hide Panel", "Resources"]);
        assert!(ViewItem::Resources.on(&rt.state));
        assert!(rt.state.panes.changes.iter().all(|c| c.cause == Cause::Menu), "{:?}", rt.state.panes.changes);
        rt.update(|s, tx| ViewItem::Resources.pick(s, tx));
        settle(&mut rt);
        assert!(target(&rt, "resources-panel").is_none() && target(&rt, "debug-panel").is_some(), "back to the platform panel");
        assert_eq!(ViewItem::ALL.map(ViewItem::id), ["view-device-list", "view-panel", "view-resources"]);
        #[cfg(target_os = "macos")]
        assert_eq!(menu(&rt.state).len(), 3);
    }

    /// #559, #560: a window too narrow for the panel collapses it with a note on the stage, logs
    /// why, and brings it back once it's wide again. Narrowing first squeezes the panes, saving
    /// nothing.
    #[test]
    fn a_narrow_window_collapses_the_panel_and_widening_brings_it_back() {
        let mut rt = runtime(panes_wide_scene);
        rt.set_viewport((1000.0, 870.0), 2.0);
        settle(&mut rt);
        let (list, panel) = (target(&rt, "device-list").unwrap(), target(&rt, "debug-panel").unwrap());
        assert!(list[2] + panel[2] <= 1000.0 - STAGE_MIN + 0.5, "squeezed to fit: {list:?} {panel:?}");
        assert_eq!((rt.state.prefs.current.sidebar.width, rt.state.prefs.current.inspector.width), (340.0, 460.0), "nothing saved");
        assert!(rt.find("narrow-note").is_none() && rt.state.panes.changes.is_empty());
        rt.set_viewport((800.0, 870.0), 2.0);
        settle(&mut rt);
        assert!(target(&rt, "debug-panel").is_none(), "no room for the panel");
        assert!(target(&rt, "device-list").is_some(), "the list stays");
        assert!(rt.find("narrow-note").is_some(), "the stage says why");
        assert!(rt.state.prefs.current.inspector.visible, "the panel is still chosen");
        // Asking for it while there's no room says so, and leaves it chosen.
        rt.key(cmd_alt('i'));
        settle(&mut rt);
        assert!(rt.state.prefs.current.inspector.visible && rt.state.status.as_deref().is_some_and(|s| s.contains("too narrow")));
        rt.set_viewport((1240.0, 870.0), 2.0);
        settle(&mut rt);
        assert!(target(&rt, "debug-panel").is_some(), "back with room");
        assert!(rt.find("narrow-note").is_none());
        let logged: Vec<(Target, bool, Cause)> = rt.state.panes.changes.iter().map(|c| (c.target, c.shown, c.cause)).collect();
        assert_eq!(logged, [(Target::Inspector, false, Cause::Narrow), (Target::Inspector, true, Cause::Room)]);
        // Narrower still: the list goes too, and both come back.
        rt.set_viewport((560.0, 870.0), 2.0);
        settle(&mut rt);
        assert!(target(&rt, "device-list").is_none() && target(&rt, "sidebar-toggle").is_some());
        assert_in_place(&rt, 560.0, "both collapsed for room");
        rt.set_viewport((1240.0, 870.0), 2.0);
        settle(&mut rt);
        assert!(target(&rt, "device-list").is_some() && target(&rt, "debug-panel").is_some());
    }

    /// A pane hidden by hand stays hidden when the window narrows and widens again.
    #[test]
    fn room_brings_back_only_what_was_chosen() {
        let mut rt = runtime(crate::demo);
        rt.key(cmd_alt('i'));
        rt.set_viewport((800.0, 870.0), 2.0);
        settle(&mut rt);
        rt.set_viewport((1240.0, 870.0), 2.0);
        settle(&mut rt);
        assert!(target(&rt, "debug-panel").is_none());
        assert_eq!(rt.state.panes.changes.len(), 1, "only the key: {:?}", rt.state.panes.changes);
    }

    /// #559: a list saved hidden opens hidden, says so in the log, and the toolbar brings it back.
    #[test]
    fn a_list_saved_hidden_opens_hidden_and_the_toolbar_brings_it_back() {
        let mut store = Store::default();
        store.current.sidebar.visible = false;
        store.current.resources.visible = true;
        let mut s = crate::init();
        restore(&mut s, store);
        let restored: Vec<(Target, bool, Cause)> = s.panes.changes.iter().map(|c| (c.target, c.shown, c.cause)).collect();
        assert_eq!(restored, [(Target::Sidebar, false, Cause::Restore), (Target::Resources, true, Cause::Restore)]);
        assert_eq!(s.panes.changes[0].to_string(), "device list hidden (restore)");
        let mut rt = Runtime::new(s, crate::view::view, (1240.0, 870.0), Env::new(true, 2.0));
        rt.poll = Some(crate::poll);
        crate::demo(&mut rt.state);
        rt.frame(0.0);
        assert!(target(&rt, "device-list").is_none());
        press(&mut rt, "sidebar-toggle");
        assert!(target(&rt, "device-list").is_some());
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
