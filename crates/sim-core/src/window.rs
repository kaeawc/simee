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
    /// The display's rotation (its first `mRotation=`, as [`Rotation::from_surface`] turns it).
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
    /// Reads `dumpsys window displays`' text. Only the first display's values count (the default
    /// display is dumped first).
    pub fn parse(text: &str) -> WindowState {
        let mut s = WindowState::default();
        for field in text.split_whitespace() {
            let Some((key, value)) = field.split_once('=') else { continue };
            match key {
                "mRotation" if s.rotation.is_none() => s.rotation = surface(value).and_then(Rotation::from_surface),
                "mLastOrientation" if s.orientation.is_none() => s.orientation = value.parse().ok(),
                "mCurrentAppOrientation" if s.orientation.is_none() => s.orientation = orientation_named(value),
                "mUserRotationMode" => s.user_locked |= value == "USER_ROTATION_LOCKED" || value == "1",
                _ => {}
            }
        }
        s.package = focused_package(text, "mFocusedApp=").or_else(|| focused_package(text, "mCurrentFocus="));
        s
    }

    /// Whether the app in front fixes the display's orientation (or keeps it to one axis), so
    /// the accelerometer doesn't turn it: anything but unspecified (-1), user (2), behind (3),
    /// sensor (4), full sensor (10) and full user (13). Unknown (no `mLastOrientation`) is false.
    pub fn app_fixed(&self) -> bool {
        self.orientation.is_some_and(|o| !matches!(o, -2 | -1 | 2 | 3 | 4 | 10 | 13))
    }
}

/// `0`..`3` or `ROTATION_0`..`ROTATION_270` as `Surface.ROTATION_*`'s number.
fn surface(value: &str) -> Option<u32> {
    match value.strip_prefix("ROTATION_") {
        Some(degrees) => degrees.parse::<u32>().ok().filter(|d| d % 90 == 0).map(|d| d / 90),
        None => value.parse().ok(),
    }
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

/// Why the guest stayed at `after.rotation` after a `rotate` (None when it turned, or when its
/// rotation couldn't be read either side): the app in front's orientation, the user's rotation
/// lock, else the turn the guest won't make (upside down, which phones refuse by default).
pub fn stayed_note(before: &WindowState, after: &WindowState) -> Option<String> {
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
        assert_eq!(WindowState::parse(&settings("ROTATION_270")).rotation, Some(Rotation::Deg90), "named as ROTATION_*");
        let named = WindowState::parse("mCurrentAppOrientation=SCREEN_ORIENTATION_PORTRAIT\nmUserRotationMode=USER_ROTATION_LOCKED");
        assert_eq!((named.orientation, named.app_fixed(), named.user_locked, named.rotation), (Some(1), true, true, None));
        assert_eq!(WindowState::parse(""), WindowState::default());
        let window_only = WindowState::parse("mCurrentFocus=Window{4c2d1e0 u0 com.example.app/com.example.app.Main}\nmFocusedApp=null");
        assert_eq!(window_only.package.as_deref(), Some("com.example.app"), "the focused window when no app record");
        assert_eq!(WindowState::parse("mCurrentFocus=Window{4c2d1e0 u0 NotificationShade}").package, None);
    }

    #[test]
    fn a_rotate_the_guest_didnt_follow_says_why() {
        let launcher = WindowState::parse(LAUNCHER);
        assert_eq!(
            stayed_note(&launcher, &launcher).as_deref(),
            Some("the guest stayed upright: the app in front (`com.google.android.apps.nexuslauncher`) doesn't rotate")
        );
        let (upright, turned) = (WindowState::parse(&settings("0")), WindowState::parse(&settings("3")));
        assert_eq!(stayed_note(&upright, &turned), None, "it turned");
        assert_eq!(
            stayed_note(&turned, &turned).as_deref(),
            Some(
                "the guest stayed at 90°: it doesn't follow this turn (phones don't turn upside down, 180°, unless the app asks); rotate again"
            )
        );
        let locked = WindowState { user_locked: true, ..upright.clone() };
        assert_eq!(stayed_note(&locked, &locked).as_deref(), Some("the guest stayed upright: auto-rotate is off"));
        let anonymous = WindowState { package: None, ..launcher };
        assert_eq!(stayed_note(&anonymous, &anonymous).as_deref(), Some("the guest stayed upright: the app in front doesn't rotate"));
        assert_eq!(stayed_note(&WindowState::default(), &upright), None, "unknown before: nothing to say");
    }
}
