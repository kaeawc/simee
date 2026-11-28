//! The 3D device view (#151, #152): the selected device's body (`sim_core::body`) drawn in
//! perspective with depth and 4x MSAA, lit by one directional light and ambient, with the live
//! screen on its screen face. ⌘3 toggles it; `SIMEE_VIEW=3d` starts with it on.
//!
//! The screen texture is the flat view's own picture: the view keeps the flat screen element
//! where it always is and lays an effect slot over the stage that asks Rune for a backdrop copy
//! (`El::effect_backdrop`). The compositor ([`gpu::Scene`]) renders the body into its own MSAA and
//! depth targets, sampling the screen element's rect of that copy, and draws the result into the
//! slot, which hides the flat screen. So whatever the flat view shows (an iOS IOSurface sampled
//! in place, Android's frames, a turned panel, an iPhone Duo's screen in use) is on the body,
//! with no CPU copy of any pixel: the copy is one GPU texture copy per drawn frame. Rune can't
//! hand a compositor a `NativeSurface`'s texture yet (anomalyco/rune#2546, simee#192).
//!
//! Both of Rune's backends draw it: its native Metal backend (macOS's default) has depth
//! attachments and MSAA resolves since anomalyco/rune#2563 (simee#193), and wgpu's
//! (`SIMEE_GPU_BACKEND=wgpu`) always had them. With the view off the window draws exactly as
//! without it.
//!
//! The user moves the camera (#155, [`View`]): a drag beside the drawn screen orbits, ⇧-drag or a
//! two-finger trackpad drag pans, a mouse wheel, ⌘-scroll or a pinch zooms. A drag that starts on
//! the drawn screen is the device's (touches through the projection are #156) unless ⌥ is held;
//! the pointer is an open hand wherever a drag moves the camera. Keys and the toolbar reach the
//! saved views ([`Command`]), each an eased [`Motion`] of one [`Camera`] value; the device's own
//! [`Turn`] (flipped, spun) moves the same way. Nothing runs while nothing moves: `poll` asks for
//! frames only while a motion is on its way. One device; arranging several is #155's part 3.
//! A foldable lies flat: the desktop doesn't track the hinge's angle (#154 animates it).

// Live Rust editing (`hot`) runs the spec without a compositor.
#![cfg_attr(feature = "hot", allow(dead_code))]

mod camera;
mod gpu;
mod layout;
mod motion;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rune::{Color, Cursor, Drag, El, GesturePhase, Key, KeyEvent, MagnifyEvent, Mods, Phase, PointerEvent, RotateEvent, Tx, WheelEvent};
use sim_core::body::{self, Body, Display, Face};
use sim_core::platform::Platform;
use sim_core::rotation::Rotation;

pub use camera::{Camera, Preset, Turn};
pub use gpu::Scene;
pub use motion::{DURATION, Motion};

use crate::State;

/// The slot's `El::effect` mark.
const MARK: u32 = 152;

/// What ⌘3 says where no compositor draws the scene (live Rust editing runs without one).
pub const UNAVAILABLE: &str = "The 3D view needs the scene's compositor, which live Rust editing (the hot feature) runs without";
/// What ⌘3 says once the view is on.
pub const ON: &str = "3D view on: drag beside the device to orbit, ⇧-drag to pan, scroll or pinch to zoom; ⌘0 face on, \
                      ⌘1 ⌘2 ⌘4 ⌘5 3/4, front, side, back, ⌘F flip, ⌘[ ⌘] turn. Touches on its screen come with #156";
/// What a drag on the drawn screen says.
pub const SCREEN_DRAG: &str = "Touches on the 3D screen come with #156: drag beside the device, or hold ⌥, to move the camera";
/// What ⌘3 says for a device with no body.
pub const NO_BODY: &str = "The 3D view has bodies for phones, tablets, foldables and watches";
/// The body row of an iPhone's CarPlay display (a car head unit, `sim_core::body`'s car rows):
/// drawn while the display shows ([`body_for`]). Without that row the view stays flat.
pub const CARPLAY_BODY: &str = "CarPlay-800x480";

/// Whether this process has the scene's compositor, set once in `main` before the window opens
/// (live Rust editing has none; tests leave it false unless they set it).
static SUPPORTED: AtomicBool = AtomicBool::new(false);

pub fn set_supported(on: bool) {
    SUPPORTED.store(on, Ordering::Relaxed);
}

/// The 3D view's switch, and the view itself.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Toggle {
    /// Asked for (⌘3, `SIMEE_VIEW=3d`) where the compositor can draw it.
    pub on: bool,
    /// The compositor is there to draw it.
    pub supported: bool,
    /// The view it started in (`SIMEE_SCENE_CAMERA`).
    pub preset: Preset,
    /// The camera and the device's turn, kept while the view is off.
    pub view: View,
}

impl Toggle {
    /// From `SIMEE_VIEW` and `SIMEE_SCENE_CAMERA`; a status line when 3D was asked for and can't be drawn.
    pub fn from_env(view: Option<&str>, camera: Option<&str>, supported: bool) -> (Toggle, Option<String>) {
        let asked = view.is_some_and(|v| v.trim().eq_ignore_ascii_case("3d"));
        let preset = Preset::parse(camera);
        let t = Toggle { on: asked && supported, supported, preset, view: View::new(preset) };
        (t, (asked && !supported).then(|| UNAVAILABLE.to_string()))
    }

    /// The process's own settings.
    pub fn from_process() -> (Toggle, Option<String>) {
        let var = |k| std::env::var(k).ok();
        Toggle::from_env(var("SIMEE_VIEW").as_deref(), var("SIMEE_SCENE_CAMERA").as_deref(), SUPPORTED.load(Ordering::Relaxed))
    }

    /// ⌘3: turns it on or off, or says why it can't.
    pub fn flip(&mut self) -> Option<String> {
        if !self.supported {
            return Some(UNAVAILABLE.into());
        }
        self.on = !self.on;
        None
    }
}

/// A change of view a key or a toolbar button asks for. Each animates ([`DURATION`]) from
/// wherever the view is, interrupting any change still running.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    /// ⌘0 and the toolbar's Face on: the screen at 1:1 where the flat view draws it, the device
    /// turned back upright and to the front.
    FaceOn,
    /// A saved view of the camera (the device keeps its turn): ⌘1 3/4, ⌘2 front, ⌘4 side, ⌘5 back.
    View(Preset),
    /// ⌘F and the toolbar's Flip: the device turns over to its back, and again to its front.
    Flip,
    /// ⌘] a quarter turn clockwise in the scene, ⌘[ counter-clockwise: the body only, not the
    /// guest's orientation (Rotate turns that).
    Spin(i32),
}

impl Command {
    /// The command a window shortcut is, if any (⌘3 is the toggle, so the views skip it).
    pub fn for_key(k: &KeyEvent) -> Option<Command> {
        let Key::Command(c) = k.key else { return None };
        if k.mods.shift || k.mods.alt {
            return None;
        }
        Some(match c {
            '0' => Command::FaceOn,
            '1' => Command::View(Preset::ThreeQuarter),
            '2' => Command::View(Preset::Front),
            '4' => Command::View(Preset::Side),
            '5' => Command::View(Preset::Back),
            'f' => Command::Flip,
            '[' => Command::Spin(-1),
            ']' => Command::Spin(1),
            _ => return None,
        })
    }
}

/// What a press on the slot is moving.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    /// The camera round the body, with the pointer travel already applied (points).
    Orbit([f32; 2]),
    Pan([f32; 2]),
    /// It started on the drawn screen: the device's (#156), so the camera stays.
    Screen,
}

/// The 3D view's camera and device transform, and the input that moves them (see the module
/// docs). Each is its own [`Motion`] track (the camera, the flip, the spin), read at `clock`, the
/// window's clock at the last `poll`, so the compositor draws what `poll` last saw; a change on
/// one track never stops or re-times another.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct View {
    pub camera: Motion<Camera>,
    /// The device's [`Turn`], a track each: turned over, and rolled about the view axis.
    pub flip: Motion<f32>,
    pub spin: Motion<f32>,
    pub clock: f64,
    /// A motion was on its way at the last step, so the next one draws its end.
    moving: bool,
    grab: Option<Grab>,
    /// The slot's size, points (what a pan's points are a fraction of).
    pub slot: [f32; 2],
    /// Where the live screen was drawn last frame, slot points; none when it faces away (or
    /// nothing was drawn), so a drag anywhere moves the camera.
    pub screen_quad: Option<[[f32; 2]; 4]>,
    /// The pointer is over the drawn screen (no open hand there).
    pub over_screen: bool,
    /// The screen shown faces -z, so the camera looks from behind at every yaw (a folded book's
    /// cover screen): set from the state before a spin, which turns the way it looks.
    pub back: bool,
    /// The user's own scroll is under way: macOS momentum after it carries the camera on, but
    /// stops counting once a change of view or a grab takes over.
    scrolling: bool,
}

/// Zoom per point of a precise scroll, as Rune's Ctrl + wheel zoom: `1 + dy / 200`.
const ZOOM_PER_POINT: f32 = 1.0 / 200.0;

impl View {
    pub fn new(preset: Preset) -> View {
        View { camera: Motion::still(Camera::preset(preset)), ..View::default() }
    }

    /// The camera and the turn as of the last step.
    pub fn camera_now(&self) -> Camera {
        self.camera.at(self.clock)
    }

    pub fn turn_now(&self) -> Turn {
        self.turn_at(self.clock)
    }

    /// The device's turn at clock `now`.
    pub fn turn_at(&self, now: f64) -> Turn {
        Turn { flip: self.flip.at(now), spin: self.spin.at(now) }
    }

    /// Where the turn is going.
    fn turn_target(&self) -> Turn {
        Turn { flip: self.flip.target(), spin: self.spin.target() }
    }

    /// 1 when the camera looks at the device's front at `now`, -1 from behind (past a quarter
    /// orbit, the back view, a back-face screen): a spin about the scene's z looks the other way
    /// round from there, so a spin asked for as seen on screen turns by this sign.
    fn spin_sign(&self, now: f64) -> f32 {
        if self.camera.at(now).world([0.0, 0.0, 1.0], self.back)[2] < 0.0 { -1.0 } else { 1.0 }
    }

    /// Starts `c` at clock `now`.
    pub fn command(&mut self, c: Command, now: f64) {
        // A change of view takes over from a scroll still coasting.
        self.scrolling = false;
        match c {
            Command::FaceOn => {
                let home = self.turn_at(now).home();
                self.camera.go(Camera::preset(Preset::Face), now, DURATION);
                self.flip.go(home.flip, now, DURATION);
                self.spin.go(home.spin, now, DURATION);
            }
            Command::View(p) => self.camera.go(Camera::preset(p), now, DURATION),
            Command::Flip => self.flip.go(self.turn_target().flip().flip, now, DURATION),
            Command::Spin(q) => {
                // Clockwise as seen on screen, from whichever side the camera is on.
                let q = if self.spin_sign(now) < 0.0 { -q } else { q };
                self.spin.go(self.turn_target().spin_by(q).spin, now, DURATION);
            }
        }
    }

    /// `poll` at clock `now`: true when the frame must draw (a motion moved, or just ended).
    pub fn step(&mut self, now: f64) -> bool {
        self.clock = now;
        let moving = self.moving();
        let changed = moving || self.moving;
        self.moving = moving;
        changed
    }

    /// A motion is still on its way at the last step: `poll` asks for the next frame.
    pub fn moving(&self) -> bool {
        self.camera.moving(self.clock) || self.flip.moving(self.clock) || self.spin.moving(self.clock)
    }

    /// A press on the drawn screen, at slot point `p`.
    fn on_screen(&self, p: [f32; 2]) -> bool {
        self.screen_quad.is_some_and(|q| camera::in_quad(&q, p))
    }

    /// One phase of a drag on the slot (a [`Drag`]'s: the pointer `at` in slot points, its
    /// `travel` since the press, the slot's `size`), with `mods` held, at clock `now`: a status
    /// line for a press the camera leaves to the device.
    pub fn drag(&mut self, phase: Phase, at: [f32; 2], travel: [f32; 2], size: [f32; 2], mods: Mods, now: f64) -> Option<&'static str> {
        if phase == Phase::Down {
            let grab = if mods.shift {
                Grab::Pan([0.0; 2])
            } else if !mods.alt && self.on_screen(at) {
                Grab::Screen
            } else {
                Grab::Orbit([0.0; 2])
            };
            self.grab = Some(grab);
            if grab == Grab::Screen {
                return Some(SCREEN_DRAG);
            }
            // A grab holds the camera where it is: a running change stops there, and so does a
            // scroll's momentum.
            self.camera.set(self.camera.at(now));
            self.scrolling = false;
            return None;
        }
        let camera = self.camera.at(now);
        match self.grab {
            Some(Grab::Orbit(done)) => {
                self.camera.set(camera.orbit(travel[0] - done[0], travel[1] - done[1]));
                self.grab = Some(Grab::Orbit(travel));
            }
            Some(Grab::Pan(done)) => {
                self.camera.set(camera.pan_by(travel[0] - done[0], travel[1] - done[1], size));
                self.grab = Some(Grab::Pan(travel));
            }
            Some(Grab::Screen) | None => {}
        }
        if phase == Phase::Up {
            self.grab = None;
        }
        None
    }

    /// The camera is being dragged (the pointer is a closed hand).
    pub fn grabbing(&self) -> bool {
        matches!(self.grab, Some(Grab::Orbit(_) | Grab::Pan(_)))
    }

    /// A wheel or two-finger scroll: a trackpad's (point-precise) pans, a mouse wheel's notches
    /// or a ⌘-scroll zoom. Momentum (macOS coasting after the fingers lift) only carries on a
    /// scroll of the user's that nothing has taken over from since: after a change of view or a
    /// grab it is dropped, so it can't stop a transition partway.
    pub fn wheel(&mut self, e: &WheelEvent, now: f64) {
        if e.momentum && !self.scrolling {
            return;
        }
        self.scrolling = true;
        let camera = self.camera.at(now);
        let next = if e.mods.cmd || !e.precise {
            camera.zoom_by((1.0 + e.dy * ZOOM_PER_POINT).max(0.1).log2())
        } else {
            camera.pan_by(e.dx, e.dy, self.slot)
        };
        self.camera.set(next);
    }

    /// A pinch: zooms by its scale.
    pub fn magnify(&mut self, e: &MagnifyEvent, now: f64) {
        let camera = self.camera.at(now);
        self.camera.set(camera.zoom_by((1.0 + e.delta).max(0.1).log2()));
    }

    /// A two-finger rotation spins the device with the fingers as seen on screen, from either
    /// side, and settles on the nearest quarter turn when they lift. Only the spin: a flip still
    /// on its way lands as it would have.
    pub fn rotate(&mut self, e: &RotateEvent, now: f64) {
        let spin = self.spin.at(now);
        match e.phase {
            GesturePhase::Began => {}
            GesturePhase::Moved => self.spin.set(spin + self.spin_sign(now) * e.degrees.to_radians()),
            GesturePhase::Ended => self.spin.go(Turn { spin, flip: 0.0 }.spin_by(0).spin, now, DURATION),
        }
    }
}

/// The selected device's body: the table's row for its device type (an Apple identifier, an
/// AVD's `hw.device.name`) when it has one, else a generic body for its screen. While an iPhone's
/// CarPlay display shows in its place, the car head unit's ([`CARPLAY_BODY`]), never the iPhone's.
pub fn body_for(s: &State) -> Option<Arc<Body>> {
    let d = s.selected_info()?;
    if s.carplay_shown() {
        return body::get(CARPLAY_BODY);
    }
    match d.platform {
        Platform::Ios => body::get(&d.device_type_id).or_else(|| body::for_profile(&d.kind()?.profile())),
        Platform::Android => {
            let device = s.android.as_ref().filter(|a| a.info.udid == d.udid);
            let profile = device.and_then(|a| a.profile().ok()).or_else(|| d.kind().map(|k| k.profile()))?;
            // An unlisted foldable folds the way its AVD's hinge says (a flip, its angles).
            let config = device.and_then(|a| a.avd.as_ref());
            let hinge = config.and_then(|c| sim_core::fold::Hinge::from_config(|k| c.get(k).map(str::to_owned)));
            body::for_android_hinge(&d.device_type_id, &profile, hinge.as_ref())
        }
    }
}

/// The body to draw when the 3D view is on and the screen is showing.
pub fn active(s: &State) -> Option<Arc<Body>> {
    (s.view3d.on && s.showing_screen()).then(|| body_for(s)).flatten()
}

/// Whether the screen the 3D view shows faces -z, so its camera looks from behind (a folded
/// book's cover screen): what [`View::back`] is set from before a spin.
pub fn shows_back(s: &State) -> bool {
    active(s).and_then(|b| place(&b, s.screen_size(), s.screen_rotation()).map(|p| gpu::facing_back(&b, &p))).unwrap_or(false)
}

/// The slot laid over the stage while the 3D view is on: it covers the flat screen (still laid
/// out where it always is, so Rune draws it into the backdrop copy) and takes the pointer, so a
/// click on the 3D view doesn't land on the flat screen beneath (mapping touches through the
/// projection is #156); drags, scrolls, pinches and rotations on it move the camera ([`View`]).
/// It tells the compositor where that screen is in the same frame: the stage's padding (`inset`,
/// points) and the screen's aspect ratio, which the view centres in what's left
/// ([`screen_rect`]).
pub fn slot(inset: f32, aspect: f32, view: &View) -> El {
    let cursor = if view.grabbing() {
        Cursor::Grabbing
    } else if view.over_screen {
        Cursor::Default
    } else {
        Cursor::Grab
    };
    rune::effect(MARK)
        .key("scene-3d")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .effect_backdrop(true)
        .effect_data([inset, aspect, 0.0, 0.0])
        .cursor(cursor)
        .on_layout(|s: &mut State, r: [f32; 4]| s.view3d.view.slot = [r[2], r[3]])
        .on_drag(|s: &mut State, d: &mut Drag, tx: &mut Tx| {
            let (at, travel, size) = ([d.x, d.y], [d.dx, d.dy], [d.w, d.h]);
            if let Some(msg) = s.view3d.view.drag(d.phase, at, travel, size, tx.mods(), tx.clock()) {
                s.status = Some(msg.into());
            }
        })
        .on_pointer_move_local(|s: &mut State, p: PointerEvent, tx: &mut Tx| {
            let over = s.view3d.view.on_screen([p.x, p.y]);
            if over == s.view3d.view.over_screen {
                tx.live_only();
            }
            s.view3d.view.over_screen = over;
        })
        .on_hover_change(|s: &mut State, inside: bool, tx: &mut Tx| {
            if !inside && s.view3d.view.over_screen {
                s.view3d.view.over_screen = false;
            } else {
                tx.live_only();
            }
        })
        .on_wheel(|s: &mut State, e: WheelEvent, tx: &mut Tx| {
            s.view3d.view.wheel(&e, tx.clock());
            true
        })
        .on_magnify(|s: &mut State, e: MagnifyEvent, tx: &mut Tx| {
            s.view3d.view.magnify(&e, tx.clock());
            true
        })
        .on_rotate(|s: &mut State, e: RotateEvent, tx: &mut Tx| {
            s.view3d.view.back = shows_back(s);
            s.view3d.view.rotate(&e, tx.clock());
            true
        })
}

/// The flat screen inside a slot at `rect` (`[x, y, w, h]`, device pixels) with `scale` device
/// pixels per point, from the slot's payload: the largest rect of the screen's aspect inside
/// the stage's padding, centred, as the view lays it out.
pub fn screen_rect(rect: [f32; 4], scale: f32, [inset, aspect, ..]: [f32; 4]) -> [f32; 4] {
    let pad = inset * scale;
    let (w, h) = ((rect[2] - 2.0 * pad).max(1.0), (rect[3] - 2.0 * pad).max(1.0));
    let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { w / h };
    let (sw, sh) = if w / h > aspect { (h * aspect, h) } else { (w, w / aspect) };
    [rect[0] + pad + (w - sw) / 2.0, rect[1] + pad + (h - sh) / 2.0, sw, sh]
}

/// What the scene is cleared to: the stage's colour (the window is always dark).
pub fn background() -> Color {
    rune::tok::theme(true).bg
}

/// Which screen of the body shows the frame, and how the body lies so it reads upright.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub display: Display,
    pub face: Face,
    /// Quarter turns clockwise, as the screen is seen, that the body is rolled by: the frame's
    /// rows run along the screen's rows after this turn.
    pub quarters: u32,
    /// The hinge's angle, degrees (180 flat).
    pub hinge: f32,
}

/// How far apart two aspect ratios can be and still be one screen (Android streams frames
/// scaled to a width, and rounds).
const ASPECT_SLACK: f32 = 0.04;

fn aspect((w, h): (u32, u32)) -> f32 {
    w as f32 / h.max(1) as f32
}

fn same_shape(a: (u32, u32), b: (u32, u32)) -> bool {
    (aspect(a) / aspect(b) - 1.0).abs() < ASPECT_SLACK
}

/// Places a frame shown upright at `shown` pixels, whose panel lies turned `turn` (clockwise,
/// as the flat view turns it to show it upright), on `body`: on the screen of that shape (the
/// outer screen of a folded foldable, its inner one unfolded), the body rolled with the panel.
/// A screen stored the other way round from its frame (the iPhone Duo's inner screen) takes the
/// frame a quarter turn clockwise, as `sim_core::body::Screen` documents: the body rolls a quarter
/// counter-clockwise so the frame reads upright. A frame no screen matches goes on the first front screen.
/// The hinge is closed when the frame is a foldable's outer screen (emu streams only that one
/// folded) and flat otherwise: the desktop doesn't know a half-open angle (#154).
pub fn place(body: &Body, shown: (u32, u32), turn: Rotation) -> Option<Placement> {
    let panel = if turn.is_sideways() { (shown.1, shown.0) } else { shown };
    let hinge = |d: Display| match (d, &body.fold) {
        (Display::Outer, Some(f)) => f.hinge.range.0,
        _ => body::HingeAxis::FLAT,
    };
    let at = |s: &body::Screen, extra: u32| Placement {
        display: s.display,
        face: s.face,
        quarters: (turn.quarters() + extra) % 4,
        hinge: hinge(s.display),
    };
    let front_first = || body.screens.iter().filter(|s| s.face == Face::Front).chain(body.screens.iter().filter(|s| s.face == Face::Back));
    // The screen of exactly that size first (two screens can be close in shape: the iPhone Duo's),
    // then one of that shape (a stream scaled to a width).
    let fits = |px: (u32, u32), exact: bool| if exact { px == panel } else { same_shape(px, panel) };
    for (exact, extra) in [(true, 0), (true, 3), (false, 0), (false, 3)] {
        let turned = |s: &&body::Screen| if extra == 0 { s.px } else { (s.px.1, s.px.0) };
        if let Some(s) = front_first().find(|s| fits(turned(s), exact)) {
            return Some(at(s, extra));
        }
    }
    body.screens.iter().find(|s| s.face == Face::Front).map(|s| at(s, 0))
}

/// Where the body UV `uv` of the screen face falls in the upright frame, with the body rolled
/// `quarters` clockwise: what the shader's `turned` does, kept here to test.
#[cfg(test)]
pub fn frame_uv([u, v]: [f32; 2], quarters: u32) -> [f32; 2] {
    match quarters % 4 {
        0 => [u, v],
        1 => [1.0 - v, u],
        2 => [1.0 - u, 1.0 - v],
        _ => [v, 1.0 - u],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_starts_from_the_environment_and_explains_a_missing_compositor() {
        let still = |p| View::new(p);
        assert_eq!(
            Toggle::from_env(None, None, true),
            (Toggle { on: false, supported: true, preset: Preset::ThreeQuarter, view: still(Preset::ThreeQuarter) }, None)
        );
        assert!(Toggle::from_env(Some("3d"), None, true).0.on);
        assert!(Toggle::from_env(Some("3D"), Some("side"), true).0.on);
        let side = Toggle::from_env(None, Some("side"), true).0;
        assert_eq!((side.preset, side.view.camera_now()), (Preset::Side, Camera::preset(Preset::Side)));
        assert_eq!(Toggle::from_env(None, Some(" Face"), true).0.view.camera_now(), Camera::preset(Preset::Face));
        assert_eq!(Toggle::from_env(None, Some("back"), true).0.preset, Preset::Back);
        assert!(!Toggle::from_env(Some("flat"), None, true).0.on);
        let (t, msg) = Toggle::from_env(Some("3d"), None, false);
        assert!(!t.on);
        assert_eq!(msg.as_deref(), Some(UNAVAILABLE));
    }

    #[test]
    fn flip_turns_it_on_and_off_only_where_it_can_draw() {
        let mut t = Toggle { supported: true, ..Toggle::default() };
        assert_eq!(t.flip(), None);
        assert!(t.on);
        assert_eq!(t.flip(), None);
        assert!(!t.on);
        let mut hot = Toggle::default();
        assert_eq!(hot.flip().as_deref(), Some(UNAVAILABLE));
        assert!(!hot.on);
    }

    #[test]
    fn a_phone_frame_goes_on_its_one_screen_upright_or_turned() {
        let b = body::get("iPhone-17").unwrap();
        let p = place(&b, (1206, 2622), Rotation::Deg0).unwrap();
        assert_eq!((p.display, p.face, p.quarters, p.hinge), (Display::Main, Face::Front, 0, 180.0));
        // Turned a quarter: the flat view shows it 2622x1206 and the body rolls with the panel.
        assert_eq!(place(&b, (2622, 1206), Rotation::Deg90).unwrap().quarters, 1);
        assert_eq!(place(&b, (1206, 2622), Rotation::Deg180).unwrap().quarters, 2);
        assert_eq!(place(&b, (2622, 1206), Rotation::Deg270).unwrap().quarters, 3);
        // A scaled stream is the same screen.
        assert_eq!(place(&b, (603, 1311), Rotation::Deg0).unwrap().quarters, 0);
    }

    #[test]
    fn a_foldable_shows_the_screen_its_frame_is() {
        let b = body::get("pixel_fold").unwrap();
        let inner = b.screen(Display::Inner).unwrap().px;
        let outer = b.screen(Display::Outer).unwrap().px;
        let open = place(&b, inner, Rotation::Deg0).unwrap();
        assert_eq!((open.display, open.face), (Display::Inner, Face::Front));
        let folded = place(&b, outer, Rotation::Deg0).unwrap();
        assert_eq!((folded.display, folded.face), (Display::Outer, Face::Back));
        assert_eq!(folded.hinge, b.fold.unwrap().hinge.range.0, "folded shut");
        assert_eq!(open.hinge, 180.0);
    }

    #[test]
    fn a_screen_stored_the_other_way_round_takes_the_frame_a_quarter_clockwise() {
        let b = body::get("iPhone-Duo").unwrap();
        let (w, h) = b.screen(Display::Inner).unwrap().px;
        let p = place(&b, (h, w), Rotation::Deg0).unwrap();
        assert_eq!((p.display, p.quarters), (Display::Inner, 3));
        // sim_core::body::Screen: a face UV (u, v) samples the frame at (v, 1 - u); the frame's
        // top-left corner is the face's top-right.
        assert_eq!(frame_uv([0.25, 0.5], p.quarters), [0.5, 0.75]);
        assert_eq!(frame_uv([1.0, 0.0], p.quarters), [0.0, 0.0]);
    }

    #[test]
    fn the_screen_rect_is_the_flat_screen_centred_in_the_stage() {
        // A 1000x800 slot at (100, 50), 2 px a point, 10 points of padding: a 1:2 screen fills
        // the 760 px high content box and is centred across its 960 px.
        assert_eq!(screen_rect([100.0, 50.0, 1000.0, 800.0], 2.0, [10.0, 0.5, 0.0, 0.0]), [410.0, 70.0, 380.0, 760.0]);
        // A wide screen fills the width instead.
        assert_eq!(screen_rect([0.0, 0.0, 400.0, 400.0], 1.0, [0.0, 2.0, 0.0, 0.0]), [0.0, 100.0, 400.0, 200.0]);
    }

    #[test]
    fn frame_uv_follows_the_roll() {
        // Rolled a quarter clockwise, the screen's top-left corner is the frame's top-right.
        assert_eq!(frame_uv([0.0, 0.0], 1), [1.0, 0.0]);
        assert_eq!(frame_uv([1.0, 0.0], 1), [1.0, 1.0]);
        assert_eq!(frame_uv([0.25, 0.5], 0), [0.25, 0.5]);
        assert_eq!(frame_uv([0.0, 0.0], 2), [1.0, 1.0]);
        assert_eq!(frame_uv([0.0, 0.0], 3), [0.0, 1.0]);
    }

    fn cmd(c: char) -> KeyEvent {
        KeyEvent { key: Key::Command(c), mods: Mods { cmd: true, ..Mods::default() } }
    }

    #[test]
    fn keys_reach_face_on_the_saved_views_flip_and_spin() {
        assert_eq!(Command::for_key(&cmd('0')), Some(Command::FaceOn));
        assert_eq!(Command::for_key(&cmd('1')), Some(Command::View(Preset::ThreeQuarter)));
        assert_eq!(Command::for_key(&cmd('2')), Some(Command::View(Preset::Front)));
        assert_eq!(Command::for_key(&cmd('4')), Some(Command::View(Preset::Side)));
        assert_eq!(Command::for_key(&cmd('5')), Some(Command::View(Preset::Back)));
        assert_eq!(Command::for_key(&cmd('f')), Some(Command::Flip));
        assert_eq!(Command::for_key(&cmd('[')), Some(Command::Spin(-1)));
        assert_eq!(Command::for_key(&cmd(']')), Some(Command::Spin(1)));
        // ⌘3 is the toggle; ⌘S, ⌘L and ⌘⇧H stay the device's; plain keys are typing.
        for c in ['3', 's', 'l'] {
            assert_eq!(Command::for_key(&cmd(c)), None, "{c}");
        }
        let mut home = cmd('h');
        home.mods.shift = true;
        assert_eq!(Command::for_key(&home), None);
        let mut shifted = cmd('0');
        shifted.mods.shift = true;
        assert_eq!(Command::for_key(&shifted), None);
        assert_eq!(Command::for_key(&KeyEvent::from(Key::Text("0".into()))), None);
    }

    /// Steps `v` like `poll` does at 60 Hz from `from` to `to`, and counts the frames that drew.
    fn frames(v: &mut View, from: f64, to: f64) -> usize {
        let mut drew = 0;
        let mut t = from;
        while t <= to + 1e-9 {
            drew += usize::from(v.step(t));
            t += 1.0 / 60.0;
        }
        drew
    }

    #[test]
    fn a_still_view_draws_nothing_and_a_change_draws_only_while_it_moves() {
        let mut v = View::new(Preset::ThreeQuarter);
        assert_eq!(frames(&mut v, 0.0, 2.0), 0, "a still view asks for no frames");
        v.command(Command::FaceOn, 2.0);
        let drew = frames(&mut v, 2.0, 4.0);
        // ~250 ms at 60 Hz, plus the frame that lands it: then nothing.
        assert!((15..=17).contains(&drew), "{drew}");
        assert!(!v.moving());
        assert_eq!(v.camera_now(), Camera::preset(Preset::Face));
        assert_eq!(frames(&mut v, 4.0, 6.0), 0);
    }

    #[test]
    fn face_on_resets_the_camera_and_the_turn() {
        let mut v = View::new(Preset::Side);
        v.command(Command::Flip, 0.0);
        v.command(Command::Spin(1), 0.0);
        v.step(1.0);
        assert!(v.turn_now().flipped());
        v.camera.set(v.camera_now().orbit(30.0, 10.0).zoom_by(1.0).pan_by(20.0, 0.0, [400.0, 400.0]));
        v.command(Command::FaceOn, 1.0);
        v.step(1.1);
        assert!(v.moving(), "it animates");
        v.step(1.3);
        assert_eq!(v.camera_now(), Camera::preset(Preset::Face));
        // Front and upright: whole turns of each (it turned on round rather than back).
        let turn = v.turn_now();
        assert!(!turn.flipped() && turn == turn.home() && (turn.spin / std::f32::consts::TAU).fract() == 0.0, "{turn:?}");
    }

    #[test]
    fn a_saved_view_keeps_the_device_turned_and_flip_turns_it_back() {
        let mut v = View::new(Preset::ThreeQuarter);
        v.command(Command::Flip, 0.0);
        v.step(0.1);
        let mid = v.turn_now();
        assert!(mid.flip > 0.0 && mid.flip < std::f32::consts::PI, "mid-flip: {mid:?}");
        v.step(0.3);
        assert!(v.turn_now().flipped());
        v.command(Command::View(Preset::Front), 0.3);
        v.step(1.0);
        assert!(v.turn_now().flipped(), "the camera moved, the device didn't");
        assert_eq!(v.camera_now(), Camera::preset(Preset::Front));
        v.command(Command::Flip, 1.0);
        v.step(2.0);
        assert!(!v.turn_now().flipped());
        // Spins are quarter turns, a quick double press two of them.
        v.command(Command::Spin(1), 2.0);
        v.command(Command::Spin(1), 2.05);
        v.step(3.0);
        assert!((v.turn_now().spin + std::f32::consts::PI).abs() < 1e-5, "{:?}", v.turn_now());
    }

    #[test]
    fn a_new_view_interrupts_a_running_one_where_it_is() {
        let mut v = View::new(Preset::Front);
        v.command(Command::View(Preset::Side), 0.0);
        v.step(0.1);
        let mid = v.camera_now();
        v.command(Command::View(Preset::ThreeQuarter), 0.1);
        assert_eq!(v.camera.at(0.1), mid, "no jump");
        v.step(0.5);
        assert_eq!(v.camera_now(), Camera::preset(Preset::ThreeQuarter));
    }

    const SLOT: [f32; 2] = [600.0, 800.0];

    /// A drag from `at` by `by` points, in two moves, with `mods`.
    fn drag(v: &mut View, at: [f32; 2], by: [f32; 2], mods: Mods) -> Option<&'static str> {
        let msg = v.drag(Phase::Down, at, [0.0; 2], SLOT, mods, 0.0);
        let half = [by[0] / 2.0, by[1] / 2.0];
        v.drag(Phase::Move, at, half, SLOT, mods, 0.0);
        assert!(v.grabbing() || msg.is_some());
        v.drag(Phase::Up, at, by, SLOT, mods, 0.0);
        assert!(!v.grabbing());
        msg
    }

    #[test]
    fn a_drag_beside_the_screen_orbits_and_with_shift_pans() {
        let mut v = View::new(Preset::Front);
        v.screen_quad = Some([[200.0, 100.0], [400.0, 100.0], [400.0, 700.0], [200.0, 700.0]]);
        assert_eq!(drag(&mut v, [50.0, 400.0], [100.0, -40.0], Mods::default()), None);
        assert_eq!(v.camera_now(), Camera::preset(Preset::Front).orbit(100.0, -40.0), "the moves add up to the drag");
        let before = v.camera_now();
        let shift = Mods { shift: true, ..Mods::default() };
        drag(&mut v, [300.0, 400.0], [80.0, 40.0], shift);
        assert_eq!(v.camera_now(), before.pan_by(80.0, 40.0, SLOT), "⇧ pans, on the screen too");
    }

    #[test]
    fn a_drag_on_the_drawn_screen_is_the_devices_unless_option_is_held() {
        let mut v = View::new(Preset::Front);
        v.screen_quad = Some([[200.0, 100.0], [400.0, 100.0], [400.0, 700.0], [200.0, 700.0]]);
        assert_eq!(drag(&mut v, [300.0, 400.0], [100.0, 0.0], Mods::default()), Some(SCREEN_DRAG));
        assert_eq!(v.camera_now(), Camera::preset(Preset::Front), "the camera stayed");
        drag(&mut v, [300.0, 400.0], [100.0, 0.0], Mods { alt: true, ..Mods::default() });
        assert_eq!(v.camera_now(), Camera::preset(Preset::Front).orbit(100.0, 0.0));
        // With the screen turned away there's nothing to touch: anywhere orbits.
        v.screen_quad = None;
        assert_eq!(drag(&mut v, [300.0, 400.0], [10.0, 0.0], Mods::default()), None);
    }

    #[test]
    fn a_grab_stops_a_running_change_where_it_is() {
        let mut v = View::new(Preset::Front);
        v.command(Command::View(Preset::Side), 0.0);
        let mid = v.camera.at(0.1);
        v.drag(Phase::Down, [10.0, 10.0], [0.0; 2], SLOT, Mods::default(), 0.1);
        assert_eq!(v.camera.at(0.5), mid);
        assert!(!v.camera.moving(0.1));
    }

    #[test]
    fn scrolls_pinches_and_rotations_move_the_camera_and_the_device() {
        let mut v = View::new(Preset::Front);
        v.slot = SLOT;
        // A trackpad's two-finger scroll pans with the fingers.
        v.wheel(&WheelEvent::new(60.0, 0.0).precise(true), 0.0);
        let pan = v.camera_now().pan;
        assert!((pan[0] - 60.0 / 800.0).abs() < 1e-6 && pan[1] == 0.0, "{pan:?}");
        // A mouse wheel's notch (40 points) zooms in by log2(1.2); ⌘ makes a trackpad scroll zoom.
        v.wheel(&WheelEvent::new(0.0, 40.0), 0.0);
        assert!((v.camera_now().zoom - 1.2f32.log2()).abs() < 1e-5);
        v.wheel(&WheelEvent::new(0.0, -40.0).precise(true).mods(Mods { cmd: true, ..Mods::default() }), 0.0);
        assert!((v.camera_now().zoom - (1.2f32.log2() + 0.8f32.log2())).abs() < 1e-5);
        // A pinch to twice the size is one stop; a huge one stops at the limit.
        let before = v.camera_now().zoom;
        v.magnify(&MagnifyEvent::new(1.0), 0.0);
        assert!((v.camera_now().zoom - before - 1.0).abs() < 1e-5);
        v.magnify(&MagnifyEvent::new(1e9), 0.0);
        assert_eq!(v.camera_now().zoom, camera::ZOOM_LIMIT);
        v.magnify(&MagnifyEvent::new(-5.0), 0.0);
        assert!(v.camera_now().zoom.is_finite());
        // Two fingers turning spin the device with them, then it settles on a quarter turn.
        v.rotate(&RotateEvent::new(0.0).phase(GesturePhase::Began), 0.0);
        v.rotate(&RotateEvent::new(30.0).phase(GesturePhase::Moved), 0.0);
        v.rotate(&RotateEvent::new(35.0).phase(GesturePhase::Moved), 0.0);
        assert!((v.turn_now().spin - 65f32.to_radians()).abs() < 1e-5);
        v.rotate(&RotateEvent::new(0.0).phase(GesturePhase::Ended), 0.0);
        v.step(1.0);
        assert!((v.turn_now().spin - std::f32::consts::FRAC_PI_2).abs() < 1e-5, "{:?}", v.turn_now());
    }

    /// The on-screen angle (radians, counter-clockwise from the right) of the device's top as
    /// `v`'s camera sees it at its last step.
    fn top_on_screen(v: &View, back: bool) -> f32 {
        let f = camera::Framing {
            center: [0.0; 3],
            half: [36.0, 75.0, 4.0],
            screen: [0.0, 0.0, 4.0],
            screen_h: 140.0,
            flat: [0.0, 0.0, 1000.0],
        };
        let (vp, _) = v.camera_now().solve(&f, (1200, 1200), back);
        let m = v.turn_now().matrix([0.0; 3]);
        let c = camera::apply(&vp, camera::apply(&m, [0.0; 3]));
        let top = camera::apply(&vp, camera::apply(&m, [0.0, 75.0, 0.0]));
        (top[1] - c[1]).atan2(top[0] - c[0])
    }

    /// How far it turned from `a` to `b` on screen, counter-clockwise positive, the short way.
    fn turned(a: f32, b: f32) -> f32 {
        use std::f32::consts::{PI, TAU};
        (b - a + PI).rem_euclid(TAU) - PI
    }

    /// The views a spin is checked from: (name, camera, the screen shown faces the back).
    fn spin_views() -> [(&'static str, Camera, bool); 4] {
        [
            ("front", Camera::preset(Preset::Front), false),
            ("⌘5 back", Camera::preset(Preset::Back), false),
            ("orbited 135°", Camera { yaw: 135f32.to_radians(), ..Camera::preset(Preset::Front) }, false),
            ("a folded book's cover screen", Camera::preset(Preset::Front), true),
        ]
    }

    #[test]
    fn spin_keys_turn_the_way_they_say_on_screen_from_any_side() {
        for (name, cam, back) in spin_views() {
            let mut v = View::new(Preset::Front);
            v.camera.set(cam);
            v.back = back;
            let before = top_on_screen(&v, back);
            v.command(Command::Spin(1), 0.0);
            v.step(1.0);
            let after = top_on_screen(&v, back);
            assert!(turned(before, after) < -0.5, "{name}: ⌘] turns clockwise: {before} -> {after}");
            v.command(Command::Spin(-1), 1.0);
            v.command(Command::Spin(-1), 1.05);
            v.step(2.0);
            let ccw = top_on_screen(&v, back);
            assert!(turned(before, ccw) > 0.5, "{name}: ⌘[ ⌘[ ends counter-clockwise of the start: {before} -> {ccw}");
        }
    }

    #[test]
    fn a_trackpad_rotation_turns_the_device_with_the_fingers_from_any_side() {
        for (name, cam, back) in spin_views() {
            let mut v = View::new(Preset::Front);
            v.camera.set(cam);
            v.back = back;
            let before = top_on_screen(&v, back);
            // Counter-clockwise fingers (RotateEvent's degrees are positive counter-clockwise).
            v.rotate(&RotateEvent::new(0.0).phase(GesturePhase::Began), 0.0);
            v.rotate(&RotateEvent::new(30.0).phase(GesturePhase::Moved), 0.0);
            let after = top_on_screen(&v, back);
            assert!(turned(before, after) > 0.1, "{name}: the device follows the fingers: {before} -> {after}");
        }
    }

    #[test]
    fn a_rotation_during_a_flip_leaves_the_flip_to_land() {
        let mut v = View::new(Preset::Front);
        v.command(Command::Flip, 0.0);
        // Two fingers turn 20° and lift while it's still turning over.
        v.rotate(&RotateEvent::new(0.0).phase(GesturePhase::Began), 0.05);
        v.rotate(&RotateEvent::new(20.0).phase(GesturePhase::Moved), 0.1);
        v.rotate(&RotateEvent::new(0.0).phase(GesturePhase::Ended), 0.15);
        v.step(1.0);
        let t = v.turn_now();
        assert!((t.flip - std::f32::consts::PI).abs() < 1e-5, "turned all the way over, not left edge on: {t:?}");
        assert!(t.spin.abs() < 1e-5, "20° settles back on the nearest quarter: {t:?}");
        // And a ⌘] mid-flip doesn't re-time the flip: each lands on its own schedule.
        let mut v = View::new(Preset::Front);
        v.command(Command::Flip, 0.0);
        let mid = v.turn_at(0.2);
        v.command(Command::Spin(1), 0.1);
        assert_eq!(v.turn_at(0.2).flip, mid.flip, "the flip runs on as it was");
    }

    #[test]
    fn scroll_momentum_continues_a_pan_but_never_freezes_a_view_change() {
        let mut v = View::new(Preset::ThreeQuarter);
        v.slot = SLOT;
        let coast = || {
            let mut e = WheelEvent::new(20.0, 0.0).precise(true);
            e.momentum = true;
            e
        };
        // A flick: the fingers pan, then the pan coasts on.
        v.wheel(&WheelEvent::new(40.0, 0.0).precise(true), 0.0);
        v.wheel(&coast(), 0.02);
        assert!((v.camera_now().pan[0] - 60.0 / 800.0).abs() < 1e-6, "momentum carries the user's pan on: {:?}", v.camera_now());
        // ⌘0 while it's still coasting: the rest of the coast leaves the change alone.
        v.command(Command::FaceOn, 0.05);
        v.wheel(&coast(), 0.1);
        v.wheel(&coast(), 0.15);
        v.step(1.0);
        assert_eq!(v.camera_now(), Camera::preset(Preset::Face));
        // Momentum with no gesture of the user's behind it does nothing either.
        let mut still = View::new(Preset::Front);
        still.wheel(&coast(), 0.0);
        assert_eq!(still.camera_now(), Camera::preset(Preset::Front));
    }
}
