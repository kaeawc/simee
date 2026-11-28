//! What an Android guest's window manager says about its display's rotation (`adb shell dumpsys
//! window displays`), and why a `rotate` it didn't follow left it where it was.
//!
//! The emulator's `rotate` turns only the virtual accelerometer; the guest decides whether to
//! follow it. It stays put when the app in front fixes its orientation (the Pixel launcher is
//! `nosensor`, `mLastOrientation=5`), when auto-rotate is off, or when the turn is upside down
//! (180°), which phones refuse unless the app asks. [`stayed_note`] says which, so a rotate that
//! changed nothing doesn't look broken.

use crate::rotation::Rotation;

/// The default display's rotation state, from `dumpsys window displays`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowState {
    /// The display's rotation (`DisplayRotation`'s `mRotation=`, a number, as
    /// [`Rotation::from_surface`] turns it).
    pub rotation: Option<Rotation>,
    /// The package of the focused app (`mFocusedApp=ActivityRecord{… u0 <package>/<activity> …}`,
    /// else the focused window's).
    pub package: Option<String>,
    /// The orientation the app in front asks for: `ActivityInfo.screenOrientation`'s number
    /// (`mLastOrientation=5`, or `mCurrentAppOrientation=SCREEN_ORIENTATION_NOSENSOR` named).
    pub orientation: Option<i32>,
    /// Whether the user locked the rotation (auto-rotate off: `mUserRotationMode=USER_ROTATION_LOCKED`).
    pub user_locked: bool,
}

impl WindowState {
    /// Reads `dumpsys window displays`' text. Only the first display counts (the default display
    /// is dumped first), and of it only its `DisplayRotation` block (the whole section on a guest
    /// that doesn't print one): activity configurations dumped beside it carry their own,
    /// possibly stale, `mRotation=ROTATION_n`, which is why only the number form is read.
    pub fn parse(text: &str) -> WindowState {
        let display = first_display(text);
        let mut s = WindowState::default();
        let (mut rotation, mut locked) = (None, None);
        for field in rotation_block(display).split_whitespace() {
            let Some((key, value)) = field.split_once('=') else { continue };
            match key {
                "mRotation" if rotation.is_none() => rotation = Some(value.parse::<u32>().ok().and_then(Rotation::from_surface)),
                "mLastOrientation" if s.orientation.is_none() => s.orientation = value.parse().ok(),
                "mCurrentAppOrientation" if s.orientation.is_none() => s.orientation = orientation_named(value),
                "mUserRotationMode" if locked.is_none() => locked = Some(value == "USER_ROTATION_LOCKED" || value == "1"),
                _ => {}
            }
        }
        s.rotation = rotation.flatten();
        s.user_locked = locked.unwrap_or(false);
        s.package = focused_package(display, "mFocusedApp=").or_else(|| focused_package(display, "mCurrentFocus="));
        s
    }

    /// Whether the app in front fixes the display's orientation (or keeps it to one axis), so
    /// the accelerometer doesn't turn it: anything but unspecified (-1), user (2), behind (3),
    /// sensor (4), full sensor (10) and full user (13). Unknown (no `mLastOrientation`) is false.
    pub fn app_fixed(&self) -> bool {
        self.orientation.is_some_and(|o| !matches!(o, -2 | -1 | 2 | 3 | 4 | 10 | 13))
    }
}

/// The text up to the second `Display:` section (the first display's, and the header before it).
fn first_display(text: &str) -> &str {
    let first = text.find("Display: ").map_or(0, |at| at + 1);
    text[first..].find("Display: ").map_or(text, |end| &text[..first + end])
}

/// The lines under the first `DisplayRotation` header, as deep as they are indented below it;
/// the whole text when there is no such header (guests before API 29).
fn rotation_block(text: &str) -> &str {
    let indent = |line: &str| line.len() - line.trim_start().len();
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n');
    for line in lines.by_ref() {
        offset += line.len();
        if line.trim() == "DisplayRotation" {
            let depth = indent(line);
            let start = offset;
            for inner in lines {
                if !inner.trim().is_empty() && indent(inner) <= depth {
                    break;
                }
                offset += inner.len();
            }
            return &text[start..offset];
        }
    }
    text
}

/// `SCREEN_ORIENTATION_*`'s number.
fn orientation_named(value: &str) -> Option<i32> {
    const NAMES: &[(&str, i32)] = &[
        ("UNSET", -2),
        ("UNSPECIFIED", -1),
        ("LANDSCAPE", 0),
        ("PORTRAIT", 1),
        ("USER", 2),
        ("BEHIND", 3),
        ("SENSOR", 4),
        ("NOSENSOR", 5),
        ("SENSOR_LANDSCAPE", 6),
        ("SENSOR_PORTRAIT", 7),
        ("REVERSE_LANDSCAPE", 8),
        ("REVERSE_PORTRAIT", 9),
        ("FULL_SENSOR", 10),
        ("USER_LANDSCAPE", 11),
        ("USER_PORTRAIT", 12),
        ("FULL_USER", 13),
        ("LOCKED", 14),
    ];
    let name = value.strip_prefix("SCREEN_ORIENTATION_")?;
    NAMES.iter().find(|(n, _)| *n == name).map(|&(_, o)| o).or_else(|| value.parse().ok())
}

/// The package in the first `<key>…{<hash> u<user> <package>/<activity>…}` record.
fn focused_package(text: &str, key: &str) -> Option<String> {
    let rest = &text[text.find(key)? + key.len()..];
    let record = rest.lines().next()?;
    let component = record.split_whitespace().find(|w| w.contains('/'))?;
    let package = component.split('/').next()?.trim_start_matches('{');
    let valid = !package.is_empty() && package.contains('.') && package.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    valid.then(|| package.to_owned())
}

/// Why the guest stayed at `after.rotation` after a `rotate` it was given `waited` seconds to
/// follow (None when it turned, or when its rotation couldn't be read either side): the app in
/// front's orientation, the user's rotation lock, else the turn the guest won't make (upside
/// down, which phones refuse by default) or, from upright, a guest too slow to turn in time.
pub fn stayed_note(before: &WindowState, after: &WindowState, waited: f64) -> Option<String> {
    let (Some(was), Some(now)) = (before.rotation, after.rotation) else { return None };
    if was != now {
        return None;
    }
    let at = match now {
        Rotation::Deg0 => "upright".to_owned(),
        r => format!("at {}°", r.degrees()),
    };
    let app = || after.package.as_deref().map_or_else(|| "the app in front".to_owned(), |p| format!("the app in front (`{p}`)"));
    let why = if after.app_fixed() {
        format!("{} doesn't rotate", app())
    } else if after.user_locked {
        "auto-rotate is off".to_owned()
    } else if now == Rotation::Deg0 {
        format!(
            "it didn't turn within {waited} s (a busy guest turns later; read `rotation` before rotating again), or the turn is upside down, 180°, which phones refuse unless the app asks"
        )
    } else {
        "it doesn't follow this turn (phones don't turn upside down, 180°, unless the app asks); rotate again".to_owned()
    };
    Some(format!("the guest stayed {at}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An API 35 guest's `dumpsys window displays` on the Pixel launcher (trimmed).
    const LAUNCHER: &str = "WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)
  Display: mDisplayId=0 rootTasks=2
    init=1080x2400 420dpi base=1080x2400 420dpi cur=1080x2400 app=1080x2337 rng=1080x1017-2337x2337
  mFocusedApp=ActivityRecord{9a3b2c1 u0 com.google.android.apps.nexuslauncher/.NexusLauncherActivity t3}
  mCurrentFocus=Window{4c2d1e0 u0 com.google.android.apps.nexuslauncher/com.google.android.apps.nexuslauncher.NexusLauncherActivity}
  DisplayRotation
    mCurrentAppOrientation=SCREEN_ORIENTATION_NOSENSOR
    mLastOrientation=5
    mRotation=0 mDeferredRotationPauseCount=0
    mLandscapeRotation=ROTATION_90 mSeascapeRotation=ROTATION_270
    mUserRotationMode=USER_ROTATION_FREE mUserRotation=ROTATION_0 mCameraRotationMode=0
";

    fn settings(rotation: &str) -> String {
        format!(
            "  mFocusedApp=ActivityRecord{{1f2e3d4 u0 com.android.settings/.Settings t12}}
  DisplayRotation
    mCurrentAppOrientation=SCREEN_ORIENTATION_UNSPECIFIED
    mLastOrientation=-1
    mRotation={rotation} mDeferredRotationPauseCount=0
    mUserRotationMode=USER_ROTATION_FREE mUserRotation=ROTATION_0
"
        )
    }

    #[test]
    fn reads_the_rotation_the_focused_package_and_the_apps_orientation() {
        let s = WindowState::parse(LAUNCHER);
        assert_eq!(s.rotation, Some(Rotation::Deg0));
        assert_eq!(s.package.as_deref(), Some("com.google.android.apps.nexuslauncher"));
        assert_eq!((s.orientation, s.app_fixed(), s.user_locked), (Some(5), true, false));
        let turned = WindowState::parse(&settings("3"));
        assert_eq!(
            (turned.rotation, turned.package.as_deref(), turned.app_fixed()),
            (Some(Rotation::Deg90), Some("com.android.settings"), false)
        );
        assert_eq!(WindowState::parse(&settings("ROTATION_270")).rotation, None, "a configuration's form, not DisplayRotation's");
        let named = WindowState::parse("mCurrentAppOrientation=SCREEN_ORIENTATION_PORTRAIT\nmUserRotationMode=USER_ROTATION_LOCKED");
        assert_eq!((named.orientation, named.app_fixed(), named.user_locked, named.rotation), (Some(1), true, true, None));
        assert_eq!(WindowState::parse(""), WindowState::default());
        let window_only = WindowState::parse("mCurrentFocus=Window{4c2d1e0 u0 com.example.app/com.example.app.Main}\nmFocusedApp=null");
        assert_eq!(window_only.package.as_deref(), Some("com.example.app"), "the focused window when no app record");
        assert_eq!(WindowState::parse("mCurrentFocus=Window{4c2d1e0 u0 NotificationShade}").package, None);
    }

    #[test]
    fn reads_only_the_first_displays_rotation_block() {
        // An activity configuration dumped ahead of DisplayRotation, with a field after its
        // rotation, and a second display that is turned and locked.
        let text = format!(
            "  Display: mDisplayId=0 rootTasks=2
    mLastReportedConfigurations: winConfig={{ mBounds=Rect(0, 0 - 2400, 1080) mRotation=3 mWindowingMode=fullscreen }}
{}  Display: mDisplayId=2 rootTasks=1
  DisplayRotation
    mRotation=1
    mUserRotationMode=USER_ROTATION_LOCKED
",
            settings("0")
        );
        let s = WindowState::parse(&text);
        assert_eq!((s.rotation, s.user_locked, s.package.as_deref()), (Some(Rotation::Deg0), false, Some("com.android.settings")));
        let block_ends = "  DisplayRotation\n    mLastOrientation=-1\n  mRotation=1\n";
        assert_eq!(WindowState::parse(block_ends).rotation, None, "a line no deeper than the header ends the block");
        assert_eq!(
            WindowState::parse("mRotation=1\nmLastOrientation=1").rotation,
            Some(Rotation::Deg270),
            "no DisplayRotation: the section"
        );
    }

    /// Real `adb shell dumpsys window` captures from an API 36 google_apis guest (a lane AVD on
    /// simee's emu, 2026-11-28), cut after the `DisplayRotation` block, with Settings and with the
    /// Pixel launcher in front. Display 0's configuration, with its `mRotation=ROTATION_0}`, is
    /// dumped ahead of the block in both.
    const REAL_SETTINGS: &str = include_str!("../testdata/dumpsys-window-settings-api36.txt");
    const REAL_LAUNCHER: &str = include_str!("../testdata/dumpsys-window-launcher-api36.txt");

    #[test]
    fn reads_real_api36_dumps() {
        let s = WindowState::parse(REAL_SETTINGS);
        assert_eq!(
            (s.rotation, s.package.as_deref(), s.orientation, s.app_fixed(), s.user_locked),
            (Some(Rotation::Deg0), Some("com.android.settings"), Some(-1), false, false)
        );
        let l = WindowState::parse(REAL_LAUNCHER);
        assert_eq!(
            (l.rotation, l.package.as_deref(), l.orientation, l.app_fixed(), l.user_locked),
            (Some(Rotation::Deg0), Some("com.google.android.apps.nexuslauncher"), Some(5), true, false)
        );
    }

    #[test]
    fn an_activity_config_ahead_of_the_display_block_doesnt_stand_in_for_it() {
        // The real dump's configuration before the block says ROTATION_0 (then `}`): a release
        // that prints another field after it, or another number, must not win over the block.
        for config in ["mRotation=ROTATION_270 mFoo=1}", "mRotation=ROTATION_270}", "mRotation=3 mFoo=1}"] {
            assert!(REAL_SETTINGS.contains("mRotation=ROTATION_0}"));
            let text = REAL_SETTINGS.replace("mRotation=ROTATION_0}", config);
            assert_eq!(WindowState::parse(&text).rotation, Some(Rotation::Deg0), "{config}");
        }
        // The block itself turned: the display's rotation follows it, not the stale configuration.
        let turned =
            REAL_SETTINGS.replace("    mRotation=0 mDeferredRotationPauseCount=0", "    mRotation=3 mDeferredRotationPauseCount=0");
        assert_eq!(WindowState::parse(&turned).rotation, Some(Rotation::Deg90));
    }

    #[test]
    fn a_rotate_the_guest_didnt_follow_says_why() {
        let launcher = WindowState::parse(LAUNCHER);
        assert_eq!(
            stayed_note(&launcher, &launcher, 3.0).as_deref(),
            Some("the guest stayed upright: the app in front (`com.google.android.apps.nexuslauncher`) doesn't rotate")
        );
        let (upright, turned) = (WindowState::parse(&settings("0")), WindowState::parse(&settings("3")));
        assert_eq!(stayed_note(&upright, &turned, 3.0), None, "it turned");
        assert_eq!(
            stayed_note(&turned, &turned, 3.0).as_deref(),
            Some(
                "the guest stayed at 90°: it doesn't follow this turn (phones don't turn upside down, 180°, unless the app asks); rotate again"
            )
        );
        let locked = WindowState { user_locked: true, ..upright.clone() };
        assert_eq!(stayed_note(&locked, &locked, 3.0).as_deref(), Some("the guest stayed upright: auto-rotate is off"));
        let anonymous = WindowState { package: None, ..launcher };
        assert_eq!(stayed_note(&anonymous, &anonymous, 3.0).as_deref(), Some("the guest stayed upright: the app in front doesn't rotate"));
        assert_eq!(stayed_note(&WindowState::default(), &upright, 3.0), None, "unknown before: nothing to say");
        assert_eq!(
            stayed_note(&upright, &upright, 3.0).as_deref(),
            Some(
                "the guest stayed upright: it didn't turn within 3 s (a busy guest turns later; read `rotation` before rotating again), or the turn is upside down, 180°, which phones refuse unless the app asks"
            ),
            "from upright a slow guest looks the same as a refused turn"
        );
    }
}
