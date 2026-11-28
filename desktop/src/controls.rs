//! Which controls a device gets, by platform and form factor: an iPhone or iPad its hardware
//! buttons, rotation and appearance; an Apple Watch its crown (press and turn) and side button; an
//! Apple TV the Siri Remote; Android devices Back/Home/Recents and power, a foldable its hinge
//! postures, a watch its rotary input, a TV its D-pad, an XR headset head turns and recentering,
//! a head unit only what it has.

use rune::Icon;
use sim_core::device::DeviceInfo;
use sim_core::form_factor::{FormFactor, Posture};
use sim_core::hid::Button;
use sim_core::platform::Platform;
use sim_core::xr::XrCommand;

use crate::{Action, icons};

/// One head turn from the Android XR toolbar, in radians (15°).
const LOOK_STEP: f32 = std::f32::consts::PI / 12.0;

/// One toolbar control.
pub struct Control {
    pub key: &'static str,
    pub icon: &'static Icon,
    pub label: &'static str,
    pub action: Action,
}

const fn control(key: &'static str, icon: &'static Icon, label: &'static str, action: Action) -> Control {
    Control { key, icon, label, action }
}

/// The controls for a running device, in toolbar order.
pub fn for_device(d: &DeviceInfo) -> Vec<Control> {
    let button = |key, icon, label, b| control(key, icon, label, Action::Button(b));
    let mut out = Vec::new();
    match d.platform {
        Platform::Ios => match d.form_factor.unwrap_or(FormFactor::Phone) {
            FormFactor::Watch => {
                out.push(button("home-button", &icons::HOME, "Crown press", Button::Home));
                out.push(button("side-button", &icons::LOCK, "Side button", Button::Side));
                out.push(control("crown-up", &icons::CROWN_UP, "Crown up", Action::Rotary(-1)));
                out.push(control("crown-down", &icons::CROWN_DOWN, "Crown down", Action::Rotary(1)));
            }
            FormFactor::Tv => {
                out.push(button("back-button", &icons::BACK, "Menu", Button::Menu));
                out.push(button("home-button", &icons::HOME, "TV", Button::Home));
                out.push(button("dpad-up", &icons::CHEVRON_UP, "Up", Button::DpadUp));
                out.push(button("dpad-down", &icons::CHEVRON_DOWN, "Down", Button::DpadDown));
                out.push(button("dpad-left", &icons::CHEVRON_LEFT, "Left", Button::DpadLeft));
                out.push(button("dpad-right", &icons::CHEVRON_RIGHT, "Right", Button::DpadRight));
                out.push(button("dpad-select", &icons::SELECT, "Select", Button::DpadCenter));
                out.push(control("appearance-button", &icons::MOON, "Appearance", Action::ToggleAppearance));
            }
            FormFactor::Vision => {
                out.push(button("home-button", &icons::HOME, "Home", Button::Home));
                out.push(control("appearance-button", &icons::MOON, "Appearance", Action::ToggleAppearance));
            }
            _ => {
                out.push(button("home-button", &icons::HOME, "Home", Button::Home));
                out.push(button("lock-button", &icons::LOCK, "Lock", Button::Lock));
                out.push(control("rotate-button", &icons::ROTATE, "Rotate", Action::Rotate));
                out.push(control("appearance-button", &icons::MOON, "Appearance", Action::ToggleAppearance));
                out.extend(apple_fold(d));
            }
        },
        Platform::Android => {
            let form_factor = d.form_factor.unwrap_or(FormFactor::Phone);
            out.push(button("back-button", &icons::BACK, "Back", Button::Back));
            out.push(button("home-button", &icons::HOME, "Home", Button::Home));
            if matches!(form_factor, FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable) {
                out.push(button("recents-button", &icons::RECENTS, "Recents", Button::Recents));
            }
            if form_factor != FormFactor::Auto {
                out.push(button("lock-button", &icons::LOCK, "Power", Button::Lock));
            }
            match form_factor {
                FormFactor::Phone => out.push(control("rotate-button", &icons::ROTATE, "Rotate", Action::Rotate)),
                FormFactor::Tablet => {
                    out.push(control("rotate-button", &icons::ROTATE, "Rotate", Action::Rotate));
                    out.push(control("stylus-button", &icons::PEN, "Stylus", Action::ToggleStylus));
                }
                FormFactor::Foldable => {
                    out.push(control("rotate-button", &icons::ROTATE, "Rotate", Action::Rotate));
                    out.push(control("fold-closed", &icons::FOLD_CLOSED, "Fold", Action::Fold(Posture::Closed)));
                    out.push(control("fold-half", &icons::FOLD_HALF, "Half open", Action::Fold(Posture::HalfOpened)));
                    out.push(control("fold-opened", &icons::FOLD_OPEN, "Unfold", Action::Fold(Posture::Opened)));
                }
                FormFactor::Watch => {
                    out.push(control("crown-up", &icons::CROWN_UP, "Rotary up", Action::Rotary(-1)));
                    out.push(control("crown-down", &icons::CROWN_DOWN, "Rotary down", Action::Rotary(1)));
                }
                FormFactor::Tv => {
                    out.push(button("dpad-up", &icons::CHEVRON_UP, "D-pad up", Button::DpadUp));
                    out.push(button("dpad-down", &icons::CHEVRON_DOWN, "D-pad down", Button::DpadDown));
                    out.push(button("dpad-left", &icons::CHEVRON_LEFT, "D-pad left", Button::DpadLeft));
                    out.push(button("dpad-right", &icons::CHEVRON_RIGHT, "D-pad right", Button::DpadRight));
                    out.push(button("dpad-select", &icons::SELECT, "Select", Button::DpadCenter));
                }
                // Android XR: turn the head 15° (about +y is to the left) and recenter.
                FormFactor::Vision => {
                    let turn = |y| Action::Xr(XrCommand::Rotate { x: 0.0, y, z: 0.0 });
                    out.push(control("xr-look-left", &icons::CHEVRON_LEFT, "Look left", turn(LOOK_STEP)));
                    out.push(control("xr-recenter", &icons::SELECT, "Recenter", Action::Xr(XrCommand::Recenter)));
                    out.push(control("xr-look-right", &icons::CHEVRON_RIGHT, "Look right", turn(-LOOK_STEP)));
                }
                FormFactor::Auto => {}
            }
        }
    }
    out.push(control("screenshot-button", &icons::CAMERA, "Screenshot", Action::Screenshot));
    out.push(control("shutdown-button", &icons::POWER, "Shut Down", Action::Shutdown));
    out
}

/// An iPhone Duo's hinge: folded (the outer screen), the book posture and open flat (the inner
/// screen). The view follows the screen in use.
fn apple_fold(d: &DeviceInfo) -> Vec<Control> {
    if !sim_core::duo::folds(&d.device_type_id) {
        return Vec::new();
    }
    vec![
        control("fold-closed", &icons::FOLD_CLOSED, "Fold", Action::Fold(Posture::Closed)),
        control("fold-half", &icons::FOLD_HALF, "Half open", Action::Fold(Posture::HalfOpened)),
        control("fold-opened", &icons::FOLD_OPEN, "Unfold", Action::Fold(Posture::Opened)),
    ]
}

/// How a running device takes drags that start on its bezel (`crate::bezel`): an iPhone, iPad or
/// Apple Watch as touches from that edge (home, the app switcher, Notification and Control
/// Center); an Android phone, tablet, foldable or watch as touches that start on the edge (gesture
/// navigation's back and home, a watch's dismiss). TVs, Vision Pro, XR headsets and cars have no
/// edge gestures, so no bezel.
pub fn bezel(d: &DeviceInfo) -> Option<crate::bezel::Mode> {
    let form_factor = d.form_factor.unwrap_or(FormFactor::Phone);
    let edges = matches!(form_factor, FormFactor::Phone | FormFactor::Tablet | FormFactor::Watch | FormFactor::Foldable);
    match d.platform {
        Platform::Ios if edges => Some(crate::bezel::Mode::Ios),
        Platform::Android if edges => Some(crate::bezel::Mode::Android),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::device::DeviceState;
    use sim_core::form_factor::Kind;

    fn device(platform: Platform, form_factor: FormFactor) -> DeviceInfo {
        DeviceInfo {
            udid: "x".into(),
            name: "x".into(),
            state: DeviceState::Booted,
            runtime_id: String::new(),
            device_type_id: String::new(),
            available: true,
            platform,
            form_factor: None,
            sub_platform: None,
        }
        .with_kind(Kind::new(platform, form_factor).ok())
    }

    fn keys(platform: Platform, form_factor: FormFactor) -> Vec<&'static str> {
        for_device(&device(platform, form_factor)).iter().map(|c| c.key).collect()
    }

    #[test]
    fn each_form_factor_gets_its_own_controls() {
        let ios = ["home-button", "lock-button", "rotate-button", "appearance-button", "screenshot-button", "shutdown-button"];
        assert_eq!(keys(Platform::Ios, FormFactor::Phone), ios);
        assert_eq!(keys(Platform::Ios, FormFactor::Tablet), ios);
        let apple = |f| keys(Platform::Ios, f);
        assert_eq!(
            apple(FormFactor::Watch),
            ["home-button", "side-button", "crown-up", "crown-down", "screenshot-button", "shutdown-button"]
        );
        assert!(apple(FormFactor::Tv).contains(&"dpad-select") && !apple(FormFactor::Tv).contains(&"lock-button"));
        assert_eq!(apple(FormFactor::Vision), ["home-button", "appearance-button", "screenshot-button", "shutdown-button"]);
        // Every Apple button control names a button that form factor has.
        for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Watch, FormFactor::Tv, FormFactor::Vision] {
            for c in for_device(&device(Platform::Ios, f)) {
                if let Action::Button(b) = c.action {
                    assert!(b.apple_press(Some(f)).is_some() || (f == FormFactor::Tv && b.apple_remote_key().is_some()), "{f:?}: {b:?}");
                }
            }
        }
        let android = |f| keys(Platform::Android, f);
        assert_eq!(
            android(FormFactor::Phone),
            ["back-button", "home-button", "recents-button", "lock-button", "rotate-button", "screenshot-button", "shutdown-button"]
        );
        assert_eq!(
            android(FormFactor::Tablet),
            [
                "back-button",
                "home-button",
                "recents-button",
                "lock-button",
                "rotate-button",
                "stylus-button",
                "screenshot-button",
                "shutdown-button"
            ]
        );
        assert!(android(FormFactor::Foldable).ends_with(&[
            "fold-closed",
            "fold-half",
            "fold-opened",
            "screenshot-button",
            "shutdown-button"
        ]));
        assert!(android(FormFactor::Watch).contains(&"crown-up") && !android(FormFactor::Watch).contains(&"recents-button"));
        assert!(android(FormFactor::Tv).contains(&"dpad-select") && !android(FormFactor::Tv).contains(&"rotate-button"));
        assert_eq!(android(FormFactor::Auto), ["back-button", "home-button", "screenshot-button", "shutdown-button"]);
        assert_eq!(
            android(FormFactor::Vision),
            [
                "back-button",
                "home-button",
                "lock-button",
                "xr-look-left",
                "xr-recenter",
                "xr-look-right",
                "screenshot-button",
                "shutdown-button"
            ]
        );
        // Every Android button control names a button Android has.
        for f in FormFactor::ALL {
            for c in for_device(&device(Platform::Android, f)) {
                if let Action::Button(b) = c.action {
                    assert!(b.android_key().is_some(), "{f:?}: {b:?}");
                }
            }
        }
    }

    #[test]
    fn the_iphone_duo_gets_its_hinge() {
        let mut duo = device(Platform::Ios, FormFactor::Foldable);
        duo.device_type_id = "com.apple.CoreSimulator.SimDeviceType.iPhone-Duo".into();
        let keys: Vec<&str> = for_device(&duo).iter().map(|c| c.key).collect();
        assert!(keys.ends_with(&["fold-closed", "fold-half", "fold-opened", "screenshot-button", "shutdown-button"]), "{keys:?}");
        assert!(!self::keys(Platform::Ios, FormFactor::Phone).contains(&"fold-closed"), "other iPhones don't fold");
    }

    #[test]
    fn devices_with_edge_gestures_get_a_bezel() {
        use crate::bezel::Mode;
        let bezel = |p, f| bezel(&device(p, f));
        for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Watch] {
            assert_eq!(bezel(Platform::Ios, f), Some(Mode::Ios), "{f:?}");
        }
        for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Foldable, FormFactor::Watch] {
            assert_eq!(bezel(Platform::Android, f), Some(Mode::Android), "{f:?}");
        }
        for (p, f) in [(Platform::Ios, FormFactor::Tv), (Platform::Ios, FormFactor::Vision), (Platform::Android, FormFactor::Tv)] {
            assert_eq!(bezel(p, f), None, "{p:?} {f:?}");
        }
        assert_eq!(bezel(Platform::Android, FormFactor::Auto), None);
        assert_eq!(bezel(Platform::Android, FormFactor::Vision), None);
    }
}
