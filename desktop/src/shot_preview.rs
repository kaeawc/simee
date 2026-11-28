//! The screenshot preview (#361), as macOS screenshots and Simulator.app show one: ⌘S, or the
//! toolbar's Screenshot, saves the focused device's shot as before, then a thumbnail of it slides
//! up into the window's bottom-right corner. It stays [`LINGER`] seconds, longer while the
//! pointer is over it, then slides off to the right. Quick shots stack leftwards, newest in the
//! corner.
//!
//! - **Drag** it into Finder, Mail, Slack or a browser's upload field: it drops as the PNG file
//!   (Rune's `drag_out`, an AppKit drag session carrying the file URL and the PNG data). A drop
//!   another app takes dismisses it, as macOS does.
//! - **Click** opens the file in its default app (Preview).
//! - **Right-click** offers Open, Copy (the PNG to the pasteboard), Show in Finder and Delete.
//! - **Swipe** it right with two fingers to dismiss it early; the file stays saved.
//!
//! iOS and Android shots share the path: whichever thread wrote the PNG posts it to the
//! [`Inbox`], and `poll` turns it into a preview ([`Previews::step`]). Everything here moves on
//! the window's clock, read in `poll`: [`Previews`] is a plain state machine, so tests drive it
//! with a fake clock. The window asks for frames only while a preview moves, and for one wake at
//! the next expiry.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rune::tok::{self, Theme};
use rune::{
    ClipboardContent, ClipboardImage, Color, Cx, DragItem, DragOutcome, El, GesturePhase, ImageCache, Pixels, PointerPress, PopupItem,
    PopupMenu, SurfaceFit, Tx, Waker, WheelEvent, el,
};

use crate::State;
use crate::work::Outbox;

/// Seconds a preview takes to slide up into place.
pub const ENTER: f64 = 0.35;
/// Seconds a preview stays before it leaves on its own.
pub const LINGER: f64 = 5.0;
/// Seconds a preview takes to slide away.
pub const EXIT: f64 = 0.3;
/// Seconds a preview stays at least once the pointer leaves it.
pub const RESUME: f64 = 1.5;
/// Previews shown at most: a further shot sends the oldest away.
pub const MAX_SHOWN: usize = 4;
/// Points a two-finger swipe to the right moves a preview before it's dismissed.
pub const SWIPE_DISMISS: f32 = 48.0;
/// The largest preview, in points: a shot is fitted into it, keeping its shape.
const BOX: (f32, f32) = (200.0, 170.0);
/// From the window's edges, and between stacked previews, in points.
const MARGIN: f32 = 16.0;
const GAP: f32 = 10.0;
/// How far past its own edge a preview travels to be off the window.
const OFFSTAGE: f32 = 24.0;

/// A shot that a saving thread wrote.
struct Saved {
    path: PathBuf,
    png: Arc<[u8]>,
}

/// Where the threads that save shots leave them for `poll`; posting wakes the window.
#[derive(Clone, Default)]
pub struct Inbox {
    shots: Arc<Mutex<Vec<Saved>>>,
    waker: Option<Waker>,
}

impl Inbox {
    /// Leaves the shot saved at `path` (its PNG is `png`) and wakes the window.
    pub fn post(&self, path: PathBuf, png: impl Into<Arc<[u8]>>) {
        if let Ok(mut shots) = self.shots.lock() {
            shots.push(Saved { path, png: png.into() });
        }
        if let Some(w) = &self.waker {
            w();
        }
    }

    /// Reads the PNG a device wrote at `path` (an Android emulator writes its own) and posts it.
    /// Call it off the UI thread; a file that can't be read shows no preview.
    pub fn post_file(&self, path: &Path) {
        if let Ok(png) = std::fs::read(path) {
            self.post(path.to_path_buf(), png);
        }
    }

    fn take(&self) -> Vec<Saved> {
        self.shots.lock().map(|mut s| std::mem::take(&mut *s)).unwrap_or_default()
    }
}

/// What a preview's file actions do. [`System`] opens and reveals through Rune (`NSWorkspace`,
/// off the UI thread) and deletes on a thread of its own; tests record them instead.
pub trait Opener {
    /// Opens the file in its default app (Preview, for a PNG).
    fn open(&self, path: &Path, tx: &mut Tx) -> Result<(), String>;
    /// Selects the file in Finder.
    fn reveal(&self, path: &Path, tx: &mut Tx) -> Result<(), String>;
    /// Deletes the file, off the UI thread; what happened goes to `outbox`.
    fn delete(&self, path: &Path, outbox: &Outbox);
}

/// The system's file actions.
pub struct System;

impl Opener for System {
    fn open(&self, path: &Path, tx: &mut Tx) -> Result<(), String> {
        tx.open_path(path).map_err(|e| format!("couldn't open {}: {e}", path.display()))
    }

    fn reveal(&self, path: &Path, tx: &mut Tx) -> Result<(), String> {
        tx.reveal_path(path).map_err(|e| format!("couldn't show {} in Finder: {e}", path.display()))
    }

    fn delete(&self, path: &Path, outbox: &Outbox) {
        let (path, outbox) = (path.to_path_buf(), outbox.clone());
        let spawned = std::thread::Builder::new().name("simee-shot-delete".into()).spawn({
            let (path, outbox) = (path.clone(), outbox.clone());
            move || {
                // To the Trash, as macOS's own screenshot preview does, so a mistaken Delete can be
                // undone; macOS 15+ ships `/usr/bin/trash`. Without it, the file is removed.
                let trashed = std::process::Command::new("/usr/bin/trash")
                    .arg(&path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                outbox.post(if trashed {
                    format!("moved {} to the Trash", path.display())
                } else {
                    match std::fs::remove_file(&path) {
                        Ok(()) => format!("deleted {}", path.display()),
                        Err(e) => format!("couldn't delete {}: {e}", path.display()),
                    }
                })
            }
        });
        if let Err(e) = spawned {
            outbox.post(format!("couldn't delete {}: {e}", path.display()));
        }
    }
}

/// A value easing from one place to another over a set time.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tween {
    from: f32,
    to: f32,
    since: f64,
    dur: f64,
}

impl Tween {
    fn at(v: f32) -> Tween {
        Tween { from: v, to: v, since: 0.0, dur: 0.0 }
    }

    fn value(&self, now: f64) -> f32 {
        if self.dur <= 0.0 {
            return self.to;
        }
        let p = ((now - self.since) / self.dur).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * ease_out(p)
    }

    /// Eases toward `to` from wherever it is now.
    fn go(&mut self, to: f32, now: f64, dur: f64) {
        if to != self.to {
            *self = Tween { from: self.value(now), to, since: now, dur };
        }
    }

    fn moving(&self, now: f64) -> bool {
        self.dur > 0.0 && self.from != self.to && now < self.since + self.dur
    }
}

/// Cubic ease-out: fast away, settling gently, as the macOS preview arrives.
fn ease_out(p: f64) -> f32 {
    (1.0 - (1.0 - p).powi(3)) as f32
}

/// Cubic ease-in: leaves slowly, then quickly.
fn ease_in(p: f64) -> f32 {
    p.powi(3) as f32
}

struct Entry {
    id: u64,
    path: PathBuf,
    png: Arc<[u8]>,
    thumb: Pixels,
    /// Its size on screen, in points.
    size: (f32, f32),
    born: f64,
    /// When it leaves on its own (pushed back while paused).
    deadline: f64,
    /// When it started leaving.
    leaving: Option<f64>,
    /// Points from the corner's margin to its right edge: its place in the stack.
    slot: Tween,
    /// Points a two-finger swipe moved it right.
    swipe: Tween,
}

/// Where a preview is drawn this frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub id: u64,
    /// Insets from the window's right and bottom edges, in points.
    pub right: f32,
    pub bottom: f32,
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
}

/// The previews shown, oldest first, and what holds them.
pub struct Previews {
    entries: Vec<Entry>,
    next: u64,
    /// The window's clock at the last [`Previews::step`]: what the view draws at.
    now: f64,
    hovered: Option<u64>,
    /// A preview's menu is open or it's being dragged out.
    held: Option<u64>,
    paused_at: Option<f64>,
    inbox: Inbox,
    cache: Option<ImageCache>,
    opener: Box<dyn Opener>,
}

impl Default for Previews {
    fn default() -> Previews {
        Previews::with_opener(Box::new(System))
    }
}

impl Previews {
    pub fn with_opener(opener: Box<dyn Opener>) -> Previews {
        Previews {
            entries: Vec::new(),
            next: 1,
            now: 0.0,
            hovered: None,
            held: None,
            paused_at: None,
            inbox: Inbox::default(),
            cache: None,
            opener,
        }
    }

    /// Wakes the window when a shot arrives or a thumbnail finishes decoding.
    pub fn connect(&mut self, waker: Waker) {
        self.inbox.waker = Some(waker.clone());
        self.cache = Some(ImageCache::new(waker));
    }

    /// Where saving threads post their shots.
    pub fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    pub fn ids(&self) -> Vec<u64> {
        self.entries.iter().map(|e| e.id).collect()
    }

    /// The previews not leaving, oldest first.
    pub fn shown(&self) -> Vec<u64> {
        self.entries.iter().filter(|e| e.leaving.is_none()).map(|e| e.id).collect()
    }

    pub fn path(&self, id: u64) -> Option<&Path> {
        self.entry(id).map(|e| e.path.as_path())
    }

    fn entry(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn entry_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Shows the shot saved at `path` (`png`, `px` pixels), drawn from `thumb`, sliding up from
    /// `now`. Returns its id.
    pub fn add(&mut self, path: PathBuf, png: Arc<[u8]>, thumb: Pixels, px: (u32, u32), now: f64) -> u64 {
        let id = self.next;
        self.next += 1;
        let size = fit(px);
        let mut deadline = now + ENTER + LINGER;
        if let Some(at) = self.paused_at {
            // Arriving while paused: it waits with the rest, as if it came when they stopped.
            deadline -= now - at;
        }
        self.entries.push(Entry {
            id,
            path,
            png,
            thumb,
            size,
            born: now,
            deadline,
            leaving: None,
            slot: Tween::at(0.0),
            swipe: Tween::at(0.0),
        });
        let shown = self.shown();
        if shown.len() > MAX_SHOWN {
            for old in &shown[..shown.len() - MAX_SHOWN] {
                self.dismiss(*old, now);
            }
        }
        self.restack(now, Some(id));
        id
    }

    /// Takes the shots the inbox has: each decodes its thumbnail off the UI thread.
    fn receive(&mut self, now: f64) -> bool {
        let shots = self.inbox.take();
        let any = !shots.is_empty();
        for Saved { path, png } in shots {
            let Some(px) = sim_core::frame::png_size(&png) else { continue };
            let thumb = match &self.cache {
                Some(cache) => cache.load_scaled(png.clone(), BOX.0.max(BOX.1), 2.0).pixels().clone(),
                None => Pixels::new(),
            };
            self.add(path, png, thumb, px, now);
        }
        any
    }

    /// Moves the clock to `now`: new shots come in, expired previews start leaving, and gone
    /// ones are dropped. Returns whether the previews drawn change.
    pub fn step(&mut self, now: f64) -> bool {
        // A preview that was moving, or is now, is drawn somewhere new.
        let moved = self.moving(self.now) || self.moving(now);
        self.now = now;
        let mut changed = self.receive(now) | moved;
        if !self.paused() {
            let due: Vec<u64> = self.entries.iter().filter(|e| e.leaving.is_none() && e.deadline <= now).map(|e| e.id).collect();
            for id in due {
                self.dismiss(id, now);
                changed = true;
            }
        }
        let before = self.entries.len();
        self.entries.retain(|e| e.leaving.is_none_or(|t| now < t + EXIT));
        if self.entries.len() != before {
            changed = true;
            if self.hovered.is_some_and(|h| self.entry(h).is_none()) || self.held.is_some_and(|h| self.entry(h).is_none()) {
                self.hovered = self.hovered.filter(|h| self.entries.iter().any(|e| e.id == *h));
                self.held = self.held.filter(|h| self.entries.iter().any(|e| e.id == *h));
                self.unpause(now);
            }
        }
        changed
    }

    /// Whether anything is mid-motion at `now`.
    pub fn moving(&self, now: f64) -> bool {
        self.entries.iter().any(|e| now < e.born + ENTER || e.leaving.is_some() || e.slot.moving(now) || e.swipe.moving(now))
    }

    /// How long until the previews next need a frame: at once while one moves, at the next
    /// expiry otherwise; `None` with nothing to wait for (none shown, or held).
    pub fn wake_in(&self, now: f64) -> Option<f64> {
        if self.moving(now) {
            return Some(0.0);
        }
        if self.paused() {
            return None;
        }
        self.entries.iter().filter(|e| e.leaving.is_none()).map(|e| (e.deadline - now).max(0.0)).reduce(f64::min)
    }

    pub fn paused(&self) -> bool {
        self.hovered.is_some() || self.held.is_some()
    }

    fn pause(&mut self, now: f64) {
        self.paused_at.get_or_insert(now);
    }

    /// The pause ended: every countdown resumes where it stopped, with at least [`RESUME`] left.
    fn unpause(&mut self, now: f64) {
        if self.paused() {
            return;
        }
        let Some(at) = self.paused_at.take() else { return };
        for e in self.entries.iter_mut().filter(|e| e.leaving.is_none()) {
            e.deadline = (e.deadline + (now - at)).max(now + RESUME);
        }
    }

    /// The pointer came over preview `id` (`inside`) or left it.
    pub fn hover(&mut self, id: u64, inside: bool, now: f64) {
        if inside {
            self.hovered = Some(id);
            self.pause(now);
        } else if self.hovered == Some(id) {
            self.hovered = None;
            self.unpause(now);
        }
    }

    /// Preview `id`'s menu opened or it's being dragged (`on`), or that ended.
    pub fn hold(&mut self, id: u64, on: bool, now: f64) {
        if on {
            self.held = Some(id);
            self.pause(now);
        } else if self.held == Some(id) {
            self.held = None;
            self.unpause(now);
        }
    }

    /// Sends preview `id` away now (the file stays).
    pub fn dismiss(&mut self, id: u64, now: f64) {
        let Some(e) = self.entry_mut(id) else { return };
        if e.leaving.is_some() {
            return;
        }
        e.leaving = Some(now);
        self.restack(now, None);
    }

    /// A drag out of preview `id` ended: a drop another app took dismisses it.
    pub fn dropped(&mut self, id: u64, outcome: DragOutcome, now: f64) {
        self.hold(id, false, now);
        if outcome != DragOutcome::Cancelled {
            self.dismiss(id, now);
        }
    }

    /// A two-finger swipe over preview `id`: it follows the fingers to the right, and past
    /// [`SWIPE_DISMISS`] it's dismissed; let go short of that, it springs back. Wheel notches
    /// and momentum don't move it. Returns whether the preview took the event.
    pub fn swipe(&mut self, id: u64, e: &WheelEvent, now: f64) -> bool {
        if e.momentum || !e.precise {
            return false;
        }
        let Some(entry) = self.entry_mut(id) else { return false };
        if entry.leaving.is_some() {
            return true;
        }
        match e.phase {
            GesturePhase::Began | GesturePhase::Moved => {
                let x = (entry.swipe.value(now) + e.dx).max(0.0);
                entry.swipe = Tween::at(x);
                if x >= SWIPE_DISMISS {
                    self.dismiss(id, now);
                }
            }
            GesturePhase::Ended => entry.swipe.go(0.0, now, ENTER / 2.0),
        }
        true
    }

    /// Lays the previews not leaving out from the corner, newest first; the one `fresh` takes
    /// its place at once, the rest ease to theirs.
    fn restack(&mut self, now: f64, fresh: Option<u64>) {
        let mut right = 0.0;
        for e in self.entries.iter_mut().rev().filter(|e| e.leaving.is_none()) {
            if Some(e.id) == fresh {
                e.slot = Tween::at(right);
            } else {
                e.slot.go(right, now, ENTER);
            }
            right += e.size.0 + GAP;
        }
    }

    /// Where each preview is drawn at the last step's clock. With `reduced` motion nothing
    /// slides: previews appear in place and leave at once.
    pub fn placed(&self, reduced: bool) -> Vec<Placed> {
        let now = self.now;
        let mut out = Vec::with_capacity(self.entries.len());
        for e in &self.entries {
            if reduced && e.leaving.is_some() {
                continue;
            }
            let (w, h) = e.size;
            let rise = if reduced { 1.0 } else { ease_out(((now - e.born) / ENTER).clamp(0.0, 1.0)) };
            let leave = e.leaving.map_or(0.0, |t| ease_in(((now - t) / EXIT).clamp(0.0, 1.0)));
            let slot = if reduced { e.slot.to } else { e.slot.value(now) };
            let swipe = e.swipe.value(now);
            let right = MARGIN + slot - swipe - leave * (MARGIN + slot - swipe + w + OFFSTAGE);
            let bottom = MARGIN - (1.0 - rise) * (MARGIN + h + OFFSTAGE);
            out.push(Placed { id: e.id, right, bottom, w, h, opacity: 1.0 - 0.6 * leave });
        }
        out
    }

    /// What dragging preview `id` out carries: the file, and its PNG for apps that take
    /// pictures rather than files.
    pub fn drag_item(&self, id: u64) -> Option<DragItem> {
        let e = self.entry(id)?;
        Some(DragItem::new().file(&e.path).png(e.png.to_vec()))
    }
}

/// A shot of `px` pixels fitted into [`BOX`], in points.
fn fit((w, h): (u32, u32)) -> (f32, f32) {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    let k = (BOX.0 / w).min(BOX.1 / h);
    ((w * k).round(), (h * k).round())
}

/// A preview's file action, from its menu or a click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Open,
    Copy,
    Reveal,
    Delete,
}

impl Act {
    const MENU: [(Act, &'static str, &'static str); 4] = [
        (Act::Open, "open", "Open"),
        (Act::Copy, "copy", "Copy"),
        (Act::Reveal, "reveal", "Show in Finder"),
        (Act::Delete, "delete", "Delete"),
    ];

    fn from_id(id: &str) -> Option<Act> {
        Act::MENU.iter().find(|(_, key, _)| *key == id).map(|(a, ..)| *a)
    }
}

/// Performs `act` on preview `id`; what happened goes to the status line.
pub fn act(s: &mut State, id: u64, act: Act, tx: &mut Tx) {
    let now = tx.clock();
    let Some((path, png)) = s.previews.entry(id).map(|e| (e.path.clone(), e.png.clone())) else { return };
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    match act {
        Act::Open => match s.previews.opener.open(&path, tx) {
            Ok(()) => s.previews.dismiss(id, now),
            Err(e) => s.status = Some(e),
        },
        Act::Copy => match ClipboardImage::from_png(png) {
            Some(image) => {
                tx.copy_rich(ClipboardContent::default().with_image(image));
                s.status = Some(format!("copied {name}"));
            }
            None => s.status = Some(format!("{name} is too large to copy")),
        },
        Act::Reveal => {
            if let Err(e) = s.previews.opener.reveal(&path, tx) {
                s.status = Some(e);
            }
        }
        Act::Delete => {
            s.previews.opener.delete(&path, &s.outbox);
            s.previews.dismiss(id, now);
        }
    }
}

/// Preview `id`'s context menu, at the pointer. The preview stays while it's open.
fn menu(s: &mut State, id: u64, press: PointerPress, tx: &mut Tx) {
    s.previews.hold(id, true, tx.clock());
    let mut menu = PopupMenu::new();
    for (a, key, title) in Act::MENU {
        if a == Act::Delete {
            menu = menu.separator();
        }
        menu = menu.item(PopupItem::new(key, title));
    }
    tx.popup_menu(menu, press, move |s: &mut State, picked, tx| {
        s.previews.hold(id, false, tx.clock());
        if let Some(a) = picked.and_then(Act::from_id) {
            act(s, id, a, tx);
        }
    });
}

/// The window, with the previews over its bottom-right corner.
pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let root = crate::view::view(s, cx);
    over(root, s, cx.system_reduced_motion())
}

/// `root` (the whole window) with the previews pinned over its bottom-right corner.
pub fn over(mut root: El, s: &State, reduced: bool) -> El {
    let t = tok::theme(true);
    for p in s.previews.placed(reduced) {
        if let Some(e) = s.previews.entry(p.id) {
            root = root.child(card(&p, e, t));
        }
    }
    root
}

fn card(p: &Placed, e: &Entry, t: Theme) -> El {
    let id = p.id;
    let name = e.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    el().key(format!("shot-preview-{id}"))
        .role("button")
        .label(format!("Screenshot {name}: click to open, drag into another app, right-click for more"))
        .w(p.w)
        .h(p.h)
        .pin(None, None, Some(p.right), Some(p.bottom))
        .radius(8.0)
        .clip_rounded(8.0)
        .fill(t.card)
        .border(1.0, Color::WHITE.alpha(0.35))
        .shadow(6.0, 18.0, Color::BLACK.alpha(0.45))
        .opacity(p.opacity)
        // It moves on the window's clock: every frame lands where `placed` puts it.
        .instant_changes()
        .child(el().w_full().h_full().pixels_fit(&e.thumb, SurfaceFit::Fill))
        .on_press_tx(move |s: &mut State, tx: &mut Tx| act(s, id, Act::Open, tx))
        .on_secondary_press(move |s: &mut State, press, tx: &mut Tx| menu(s, id, press, tx))
        .on_hover_change(move |s: &mut State, inside, tx: &mut Tx| s.previews.hover(id, inside, tx.clock()))
        .on_wheel(move |s: &mut State, w: WheelEvent, tx: &mut Tx| s.previews.swipe(id, &w, tx.clock()))
        .drag_out(move |s: &State| s.previews.drag_item(id))
        .on_drag_start(move |s: &mut State, tx: &mut Tx| s.previews.hold(id, true, tx.clock()))
        .on_drag_end(move |s: &mut State, outcome, tx: &mut Tx| s.previews.dropped(id, outcome, tx.clock()))
}

/// Encodes an iOS frame as PNG and writes it to `path` on its own thread; the status line says
/// how it went, and a saved shot goes to `inbox` for its preview.
pub fn save_png(frame: sim_core::frame::Frame, path: PathBuf, outbox: Outbox, inbox: Inbox) {
    crate::work::save_png(frame, path, outbox, move |path, png| inbox.post(path, png));
}

/// Headless scenes: previews of the scene's own screen, settled in the corner, as if `n` shots
/// were taken just now.
pub fn demo_shots(s: &mut State, n: usize) {
    let (w, h) = s.pixels.size();
    let frame = sim_core::frame::Frame { width: w, height: h, rgba: s.pixels.rgba() };
    let png: Arc<[u8]> = sim_core::frame::encode_png(&frame).unwrap_or_default().into();
    for i in 0..n {
        let path = std::env::temp_dir().join(format!("simee-demo-{i}.png"));
        s.previews.add(path, png.clone(), s.pixels.clone(), (w, h), -ENTER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Records the file actions instead of performing them.
    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<Vec<String>>>);

    impl Opener for Fake {
        fn open(&self, path: &Path, _: &mut Tx) -> Result<(), String> {
            self.0.borrow_mut().push(format!("open {}", path.display()));
            Ok(())
        }
        fn reveal(&self, path: &Path, _: &mut Tx) -> Result<(), String> {
            self.0.borrow_mut().push(format!("reveal {}", path.display()));
            Err("no Finder here".into())
        }
        fn delete(&self, path: &Path, _: &Outbox) {
            self.0.borrow_mut().push(format!("delete {}", path.display()));
        }
    }

    /// A 2x1 PNG.
    fn png() -> Arc<[u8]> {
        sim_core::frame::encode_rgba(2, 1, &[255; 8]).expect("png").into()
    }

    fn shot(p: &mut Previews, name: &str, now: f64) -> u64 {
        p.add(PathBuf::from(format!("/shots/{name}.png")), png(), Pixels::new(), (1206, 2622), now)
    }

    fn at(p: &Previews, id: u64) -> Placed {
        p.placed(false).into_iter().find(|x| x.id == id).expect("drawn")
    }

    #[test]
    fn a_shot_slides_up_eased_stays_then_slides_away() {
        let mut p = Previews::default();
        let id = shot(&mut p, "a", 10.0);
        p.step(10.0);
        let start = at(&p, id);
        assert!(start.bottom + start.h < 0.0, "it starts below the window: {start:?}");
        p.step(10.0 + ENTER / 2.0);
        let mid = at(&p, id);
        // Eased out: past halfway at half time.
        assert!(mid.bottom > (start.bottom + MARGIN) / 2.0, "{mid:?}");
        assert!(p.moving(10.0 + ENTER / 2.0));
        assert_eq!(p.wake_in(10.0 + ENTER / 2.0), Some(0.0), "frames while it moves");
        p.step(10.0 + ENTER);
        let rest = at(&p, id);
        assert_eq!((rest.right, rest.bottom, rest.opacity), (MARGIN, MARGIN, 1.0), "in the corner");
        assert!(!p.moving(10.0 + ENTER));
        let wait = p.wake_in(10.0 + ENTER).expect("a wake at the expiry");
        assert!((wait - LINGER).abs() < 1e-9, "{wait}");
        // Not yet.
        p.step(10.0 + ENTER + LINGER - 0.01);
        assert_eq!(p.shown(), [id]);
        p.step(10.0 + ENTER + LINGER);
        assert!(p.shown().is_empty(), "leaving");
        assert_eq!(p.ids(), [id], "still drawn while it slides away");
        p.step(10.0 + ENTER + LINGER + EXIT / 2.0);
        assert!(at(&p, id).right < MARGIN, "it slides off to the right");
        p.step(10.0 + ENTER + LINGER + EXIT);
        assert!(p.ids().is_empty(), "gone");
        assert_eq!(p.wake_in(20.0), None, "nothing left to wake for");
    }

    #[test]
    fn hovering_pauses_the_countdown_and_leaving_resumes_it() {
        let mut p = Previews::default();
        let id = shot(&mut p, "a", 0.0);
        p.step(1.0);
        p.hover(id, true, 1.0);
        assert!(p.paused());
        p.step(30.0);
        assert_eq!(p.shown(), [id], "it stays while hovered");
        assert_eq!(p.wake_in(30.0), None, "no expiry to wake for while paused");
        p.hover(id, false, 30.0);
        // It had ENTER + LINGER - 1 s left when the pointer came.
        let left = p.wake_in(30.0).expect("counting again");
        assert!((left - (ENTER + LINGER - 1.0)).abs() < 1e-9, "{left}");
        // A pointer that leaves near the end still gives it RESUME.
        let id2 = shot(&mut p, "b", 40.0);
        p.step(40.0 + ENTER + LINGER - 0.1);
        p.hover(id2, true, 40.0 + ENTER + LINGER - 0.1);
        p.hover(id2, false, 50.0);
        p.step(50.0);
        assert!((p.wake_in(50.0).expect("wake") - RESUME).abs() < 1e-9);
        // Hover leave from a preview that isn't the hovered one changes nothing.
        p.hover(id2, true, 50.0);
        p.hover(id, false, 50.0);
        assert!(p.paused());
    }

    #[test]
    fn quick_shots_stack_leftwards_and_close_up_as_they_go() {
        let mut p = Previews::default();
        let a = shot(&mut p, "a", 0.0);
        let b = shot(&mut p, "b", 0.5);
        p.step(0.5 + ENTER);
        let (pa, pb) = (at(&p, a), at(&p, b));
        assert_eq!(pb.right, MARGIN, "the newest is in the corner");
        assert_eq!(pa.right, MARGIN + pb.w + GAP, "the older one moved aside");
        // Past the limit the oldest leave.
        let more: Vec<u64> = (0..MAX_SHOWN).map(|i| shot(&mut p, &format!("c{i}"), 2.0)).collect();
        assert_eq!(p.shown(), more, "{MAX_SHOWN} shown at most");
        p.step(2.0 + EXIT);
        assert!(p.entry(a).is_none() && p.entry(b).is_none());
        // Dismissing the corner one closes the gap: the next eases into the corner.
        let corner = *more.last().expect("newest");
        let next = more[more.len() - 2];
        p.dismiss(corner, 3.0);
        p.step(3.0 + ENTER / 2.0);
        assert!(at(&p, next).right > MARGIN, "on its way");
        p.step(3.0 + ENTER);
        assert_eq!(at(&p, next).right, MARGIN, "in the corner");
    }

    #[test]
    fn a_swipe_right_dismisses_and_a_short_one_springs_back() {
        let mut p = Previews::default();
        let id = shot(&mut p, "a", 0.0);
        p.step(ENTER);
        let wheel = |dx: f32, phase| WheelEvent::new(dx, 0.0).precise(true).phase(phase);
        p.swipe(id, &wheel(0.0, GesturePhase::Began), ENTER);
        p.swipe(id, &wheel(20.0, GesturePhase::Moved), ENTER);
        p.step(ENTER);
        assert_eq!(at(&p, id).right, MARGIN - 20.0, "it follows the fingers");
        p.swipe(id, &wheel(0.0, GesturePhase::Ended), ENTER);
        p.step(ENTER * 2.0);
        assert_eq!(at(&p, id).right, MARGIN, "back in place");
        // Leftwards doesn't move it, and a mouse wheel's notches don't either.
        assert!(p.swipe(id, &wheel(-30.0, GesturePhase::Moved), 1.0), "the preview takes its swipes");
        assert!(!p.swipe(id, &WheelEvent::new(90.0, 0.0), 1.0));
        assert_eq!(p.shown(), [id]);
        p.swipe(id, &wheel(SWIPE_DISMISS, GesturePhase::Moved), 1.0);
        assert!(p.shown().is_empty(), "flicked away");
    }

    #[test]
    fn a_drop_another_app_takes_dismisses_and_a_cancelled_one_keeps_it() {
        let mut p = Previews::default();
        let id = shot(&mut p, "a", 0.0);
        let item = p.drag_item(id).expect("draggable");
        assert_eq!(item.paths(), [PathBuf::from("/shots/a.png")]);
        assert_eq!(item.png_data(), Some(&png()[..]), "the PNG rides along");
        p.hold(id, true, 1.0);
        p.step(60.0);
        assert_eq!(p.shown(), [id], "held while dragged");
        p.dropped(id, DragOutcome::Cancelled, 60.0);
        assert!(!p.paused());
        assert_eq!(p.shown(), [id]);
        p.hold(id, true, 61.0);
        p.dropped(id, DragOutcome::Copied, 61.0);
        assert!(p.shown().is_empty());
    }

    #[test]
    fn reduced_motion_lands_in_place_and_leaves_at_once() {
        let mut p = Previews::default();
        let id = shot(&mut p, "a", 0.0);
        p.step(0.0);
        let placed = p.placed(true);
        assert_eq!((placed[0].right, placed[0].bottom), (MARGIN, MARGIN));
        p.dismiss(id, 0.1);
        p.step(0.1);
        assert!(p.placed(true).is_empty());
    }

    #[test]
    fn shots_fit_the_preview_box_keeping_their_shape() {
        assert_eq!(fit((1206, 2622)), (78.0, 170.0));
        assert_eq!(fit((2560, 1600)), (200.0, 125.0));
        assert_eq!(fit((0, 0)), (170.0, 170.0));
    }

    #[test]
    fn saved_shots_arrive_through_the_inbox_and_wake_the_window() {
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let mut p = Previews::default();
        p.connect(Arc::new(move || {
            w.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        let inbox = p.inbox();
        std::thread::spawn(move || inbox.post("/shots/a.png".into(), png())).join().expect("posted");
        // Not a PNG: no preview.
        p.inbox().post("/shots/b.png".into(), &b"nope"[..]);
        assert!(woken.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        assert!(p.step(1.0));
        assert_eq!(p.ids().len(), 1);
        assert_eq!(p.path(p.ids()[0]), Some(Path::new("/shots/a.png")));
        // Android: the emulator wrote the file; the worker reads it back.
        let file = std::env::temp_dir().join(format!("simee-preview-{}.png", std::process::id()));
        std::fs::write(&file, png()).expect("write");
        p.inbox().post_file(&file);
        p.inbox().post_file(Path::new("/nonexistent/simee.png"));
        p.step(2.0);
        assert_eq!(p.ids().len(), 2);
        let _ = std::fs::remove_file(file);
    }

    mod window {
        use super::*;
        use rune::{Env, PathAction, Runtime};

        fn runtime(fake: &Fake) -> Runtime<State> {
            let mut s = crate::init();
            crate::demo(&mut s);
            s.previews = Previews::with_opener(Box::new(fake.clone()));
            let mut rt = Runtime::new(s, view, (1240.0, 870.0), Env::new(true, 2.0));
            rt.poll = Some(crate::poll);
            rt.frame(0.0);
            rt
        }

        fn shoot(rt: &mut Runtime<State>, name: &str) -> u64 {
            let now = rt.state.previews.now;
            let id = shot(&mut rt.state.previews, name, now);
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
            id
        }

        /// A preview sits in the window's bottom-right corner over everything, and a click on
        /// it opens the file and sends it away.
        #[test]
        fn a_click_opens_the_shot_and_dismisses_it() {
            let fake = Fake::default();
            let mut rt = runtime(&fake);
            let id = shoot(&mut rt, "a");
            let key = format!("shot-preview-{id}");
            let ([x, y, w, h], _) = rt.presented(&key).expect("drawn");
            assert!((x + w - (1240.0 - MARGIN)).abs() < 0.5 && (y + h - (870.0 - MARGIN)).abs() < 0.5, "in the corner: {:?}", [x, y, w, h]);
            let info = rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key.as_str())).expect("listed");
            assert!(info.drag_out, "it's a drag source for other apps");
            let (cx, cy) = rt.find(&key).expect("hit target");
            rt.pointer_down(cx, cy);
            rt.pointer_up(cx, cy);
            rt.frame(1.0 / 60.0);
            assert_eq!(*fake.0.borrow(), ["open /shots/a.png"]);
            assert!(rt.state.previews.shown().is_empty(), "opened: it leaves");
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
            assert!(rt.find(&key).is_none(), "gone after its exit");
        }

        /// Right-click: Open, Copy, Show in Finder, Delete. Copy puts the PNG on the pasteboard;
        /// Delete removes the file (through the opener) and the preview.
        #[test]
        fn the_menu_copies_reveals_and_deletes() {
            let fake = Fake::default();
            let mut rt = runtime(&fake);
            let clipboard = rune::clipboard::memory();
            rt.use_clipboard(&clipboard);
            let id = shoot(&mut rt, "a");
            let (cx, cy) = rt.find(&format!("shot-preview-{id}")).expect("hit target");
            let open_menu = |rt: &mut Runtime<State>| {
                rt.secondary_down(cx, cy);
                rt.secondary_up(cx, cy);
                rt.frame(1.0 / 60.0);
            };
            open_menu(&mut rt);
            let titles: Vec<String> = rt
                .popup_menu()
                .expect("a menu")
                .entries()
                .iter()
                .filter_map(|e| match e {
                    rune::PopupEntry::Item(i) => Some(i.title().to_owned()),
                    _ => None,
                })
                .collect();
            assert_eq!(titles, ["Open", "Copy", "Show in Finder", "Delete"]);
            assert!(rt.state.previews.paused(), "it stays while the menu is open");
            rt.resolve_popup(Some("copy")).expect("picked");
            rt.frame(1.0 / 60.0);
            let copied = clipboard.content().and_then(|c| c.image().map(|i| i.png().to_vec()));
            assert_eq!(copied.as_deref(), Some(&png()[..]), "the PNG is on the pasteboard");
            assert_eq!(rt.state.status.as_deref(), Some("copied a.png"));
            assert!(!rt.state.previews.paused());
            open_menu(&mut rt);
            rt.resolve_popup(Some("reveal")).expect("picked");
            rt.frame(1.0 / 60.0);
            assert_eq!(rt.state.status.as_deref(), Some("no Finder here"), "a failure says so");
            open_menu(&mut rt);
            rt.resolve_popup(Some("delete")).expect("picked");
            rt.frame(1.0 / 60.0);
            assert_eq!(*fake.0.borrow(), ["reveal /shots/a.png", "delete /shots/a.png"]);
            assert!(rt.state.previews.shown().is_empty());
        }

        /// The system opener goes through Rune's `open_path` and `reveal_path`.
        #[test]
        fn the_system_opener_opens_and_reveals_through_rune() {
            let mut s = crate::init();
            crate::demo(&mut s);
            let mut rt = Runtime::new(s, view, (1240.0, 870.0), Env::new(true, 2.0));
            rt.poll = Some(crate::poll);
            rt.frame(0.0);
            let id = shoot(&mut rt, "a");
            let (cx, cy) = rt.find(&format!("shot-preview-{id}")).expect("hit target");
            rt.secondary_down(cx, cy);
            rt.secondary_up(cx, cy);
            rt.frame(1.0 / 60.0);
            rt.resolve_popup(Some("reveal")).expect("picked");
            rt.frame(1.0 / 60.0);
            rt.pointer_down(cx, cy);
            rt.pointer_up(cx, cy);
            rt.frame(1.0 / 60.0);
            let asked: Vec<(PathBuf, PathAction)> = rt.take_path_requests().into_iter().map(|r| (r.path, r.action)).collect();
            assert_eq!(asked, [(PathBuf::from("/shots/a.png"), PathAction::Reveal), (PathBuf::from("/shots/a.png"), PathAction::Open)]);
        }

        /// ⌘S takes a shot wherever the focus is, not only on the device's screen.
        #[test]
        fn cmd_s_takes_a_shot_from_anywhere_in_the_window() {
            let mut s = crate::init();
            crate::demo(&mut s);
            let mut rt = Runtime::new(s, view, (1240.0, 870.0), Env::new(true, 2.0));
            rt.on_key = Some(crate::on_key);
            rt.frame(0.0);
            rt.state.actions.clear();
            rt.key(rune::KeyEvent { key: rune::Key::Command('s'), mods: rune::Mods { cmd: true, ..rune::Mods::default() } });
            rt.frame(1.0 / 60.0);
            assert_eq!(rt.state.actions, [crate::Action::Screenshot]);
        }

        /// The pointer over a preview keeps it past its time.
        #[test]
        fn a_hovered_preview_stays() {
            let fake = Fake::default();
            let mut rt = runtime(&fake);
            let id = shoot(&mut rt, "a");
            let (cx, cy) = rt.find(&format!("shot-preview-{id}")).expect("hit target");
            rt.pointer_move(cx, cy);
            for _ in 0..(60 * (LINGER as usize + 2)) {
                rt.frame(1.0 / 60.0);
            }
            assert_eq!(rt.state.previews.shown(), [id]);
            // It came under the pointer at once: its whole time is left when the pointer goes.
            rt.pointer_move(10.0, 10.0);
            for _ in 0..(60 * (LINGER as usize - 1)) {
                rt.frame(1.0 / 60.0);
            }
            assert_eq!(rt.state.previews.shown(), [id], "counting again from where it stopped");
            for _ in 0..120 {
                rt.frame(1.0 / 60.0);
            }
            assert!(rt.state.previews.ids().is_empty(), "gone once its time ran out");
        }
    }
}
