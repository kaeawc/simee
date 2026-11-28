//! Profiles: named sets of packages turned off on a running device, over adb, the counterpart of
//! the iOS backend's sets of launchd jobs. Each package is disabled for user 0
//! (`pm disable-user --user 0`): its processes stop, none of its components starts again, and the
//! package manager keeps the state in `/data/system/users/0/package-restrictions.xml`, so it stays
//! off across reboots, and in a clone or an export of the device. Undo enables them
//! (`pm enable --user 0`), at once.
//!
//! A package the image doesn't have is reported, not failed: images differ (`google_apis` has no
//! Play Store, `google_apis_playstore` has more Google apps).

use crate::apps::Adb;

/// A named set of packages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    /// Packages disabled for user 0.
    pub disabled: &'static [&'static str],
}

/// The profiles `profile` applies.
pub const PROFILES: &[Profile] = &[Profile { name: "slim", disabled: SLIM }];

/// The `slim` profile: Google packages that run their own scheduled work on a booted phone, with
/// no account signed in, and that nothing an app under test, AutoMobile or simee's own methods
/// call reaches. What each saves in settle time and memory is not measured yet
/// (docs/benchmarks.md, "Not yet measured"); an entry that saves nothing measurable comes out.
///
/// - Device Health Services (`apps.turbo`): adaptive battery and brightness models, retrained in
///   background jobs.
/// - Digital Wellbeing (`apps.wellbeing`): app-usage tracking and its dashboard.
/// - Data Restore Tool (`apps.restore`): restoring a backup after setup, which a fresh device
///   never has.
/// - Google Partner Setup (`partnersetup`): first-boot partner customisation.
///
/// Kept on purpose: the launcher, System UI, the keyboards (`com.android.inputmethod.latin`,
/// `com.google.android.inputmethod.latin`), Google Play services and its framework, the WebView,
/// the Play Store (billing, review and integrity APIs apps call), Settings, the package installer
/// and permission controller (`install`, privacy dialogs), the phone and messaging apps (the
/// console's `gsm call` and `sms send`), text-to-speech (TalkBack), Android System Intelligence
/// (text selection) and the shell.
const SLIM: &[&str] = &[
    "com.google.android.apps.turbo",
    "com.google.android.apps.wellbeing",
    "com.google.android.apps.restore",
    "com.google.android.partnersetup",
];

/// The profile named `name`.
pub fn profile(name: &str) -> Result<&'static Profile, String> {
    PROFILES.iter().find(|p| p.name == name).ok_or_else(|| {
        let names: Vec<&str> = PROFILES.iter().map(|p| p.name).collect();
        format!("unknown profile `{name}` ({})", names.join(", "))
    })
}

/// What applying (or undoing) a profile did, in the iOS backend's terms: the packages now in the
/// wanted state, those that aren't (the package manager refused), those the image doesn't have,
/// and those that were off already (counted in `changed` too).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub changed: usize,
    pub failed: Vec<String>,
    pub not_loaded: Vec<String>,
    pub already: usize,
}

/// The line between `pm list packages`'s output and `pm list packages -d`'s in one adb call.
const DISABLED_MARK: &str = "@disabled";

impl Adb {
    /// Turns `profile`'s packages off (or with `undo`, on), then reads back what the package
    /// manager says: two adb calls, one when nothing needs changing.
    pub fn apply_profile(&self, profile: &Profile, undo: bool) -> Result<Applied, String> {
        let before = self.shell(&format!("pm list packages; echo {DISABLED_MARK}; pm list packages -d"), self.deadlines.query)?;
        let (installed, disabled) = split_lists(&before);
        let todo = to_change(profile.disabled, &installed, &disabled, undo);
        let after = if todo.is_empty() {
            disabled.clone()
        } else {
            // Each is a package name from the table (checked by a test), so safe in a shell line.
            let verb = if undo { "pm enable --user 0" } else { "pm disable-user --user 0" };
            let mut line: Vec<String> = todo.iter().map(|p| format!("{verb} {p}")).collect();
            line.push(format!("echo {DISABLED_MARK}"));
            line.push("pm list packages -d".to_owned());
            // A package the manager refuses prints an error and the line goes on; the state read
            // back is what counts.
            let (_, out) = self.run(&["shell", &line.join("; ")], self.deadlines.query)?;
            let Some((_, listed)) = out.split_once(DISABLED_MARK) else {
                return Err(crate::apps::failure("pm", &out));
            };
            crate::apps::parse_packages(listed)
        };
        Ok(outcome(profile.disabled, &installed, &disabled, &after, undo))
    }
}

/// `pm list packages; echo @disabled; pm list packages -d`'s output: the installed packages and
/// the disabled ones.
fn split_lists(out: &str) -> (Vec<String>, Vec<String>) {
    let (all, disabled) = out.split_once(DISABLED_MARK).unwrap_or((out, ""));
    (crate::apps::parse_packages(all), crate::apps::parse_packages(disabled))
}

/// The packages of `wanted` that need a call: installed ones that are on (or with `undo`, off).
fn to_change<'a>(wanted: &[&'a str], installed: &[String], disabled: &[String], undo: bool) -> Vec<&'a str> {
    let has = |list: &[String], p: &str| list.iter().any(|x| x == p);
    wanted.iter().copied().filter(|p| has(installed, p) && has(disabled, p) == undo).collect()
}

/// What a profile did to `wanted`, from the package manager's lists before (`installed`,
/// `disabled`) and after (`after`, the disabled ones).
pub fn outcome(wanted: &[&str], installed: &[String], disabled: &[String], after: &[String], undo: bool) -> Applied {
    let has = |list: &[String], p: &str| list.iter().any(|x| x == p);
    let mut out = Applied::default();
    for p in wanted.iter().copied() {
        if !has(installed, p) {
            out.not_loaded.push(p.to_owned());
        } else if has(after, p) != undo {
            out.changed += 1;
            out.already += usize::from(!undo && has(disabled, p));
        } else {
            out.failed.push(p.to_owned());
        }
    }
    if undo {
        // Undo reports what it turned on, as iOS's does: no counts of what was off already.
        out.already = 0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn profiles_are_found_by_name_and_hold_only_package_names() {
        let slim = profile("slim").expect("slim");
        assert_eq!(profile("tiny").expect_err("unknown"), "unknown profile `tiny` (slim)");
        for p in slim.disabled {
            crate::apps::package_name(p).expect("a package name: safe in the shell line");
        }
        let unique: std::collections::HashSet<_> = slim.disabled.iter().collect();
        assert_eq!(unique.len(), slim.disabled.len(), "no package twice");
    }

    #[test]
    fn the_slim_profile_keeps_what_automobile_and_simee_need() {
        let slim = profile("slim").expect("slim");
        for kept in [
            "android",
            "com.android.systemui",
            "com.android.launcher3",
            "com.google.android.apps.nexuslauncher",
            "com.android.inputmethod.latin",
            "com.google.android.inputmethod.latin",
            "com.google.android.gms",
            "com.google.android.gsf",
            "com.google.android.webview",
            "com.android.webview",
            "com.android.vending",
            "com.android.settings",
            "com.android.shell",
            "com.android.packageinstaller",
            "com.google.android.packageinstaller",
            "com.android.permissioncontroller",
            "com.google.android.permissioncontroller",
            "com.android.phone",
            "com.google.android.dialer",
            "com.google.android.apps.messaging",
            "com.google.android.tts",
            "com.google.android.marvin.talkback",
            "com.google.android.as",
            "com.android.providers.settings",
            "com.android.providers.media.module",
        ] {
            assert!(!slim.disabled.contains(&kept), "{kept} is needed");
        }
    }

    #[test]
    fn an_apply_counts_what_is_off_now_and_names_what_isnt() {
        let wanted = ["a.one", "a.two", "a.three", "a.four"];
        let installed = names(&["a.one", "a.two", "a.three", "android"]);
        // a.one was off already, a.two goes off, a.three is refused, a.four isn't on this image.
        let disabled = names(&["a.one"]);
        assert_eq!(to_change(&wanted, &installed, &disabled, false), ["a.two", "a.three"]);
        let after = names(&["a.one", "a.two"]);
        assert_eq!(
            outcome(&wanted, &installed, &disabled, &after, false),
            Applied { changed: 2, failed: names(&["a.three"]), not_loaded: names(&["a.four"]), already: 1 }
        );
    }

    #[test]
    fn an_undo_counts_what_is_on_now() {
        let wanted = ["a.one", "a.two", "a.four"];
        let installed = names(&["a.one", "a.two"]);
        let disabled = names(&["a.one", "a.two"]);
        assert_eq!(to_change(&wanted, &installed, &disabled, true), ["a.one", "a.two"]);
        // a.two stays off (the manager refused).
        let after = names(&["a.two"]);
        assert_eq!(
            outcome(&wanted, &installed, &disabled, &after, true),
            Applied { changed: 1, failed: names(&["a.two"]), not_loaded: names(&["a.four"]), already: 0 }
        );
    }

    #[test]
    fn nothing_to_change_needs_no_second_call() {
        let wanted = ["a.one"];
        let on = names(&["a.one"]);
        assert!(to_change(&wanted, &on, &on, false).is_empty(), "off already");
        assert!(to_change(&wanted, &on, &[], true).is_empty(), "on already");
        assert!(to_change(&wanted, &[], &[], false).is_empty(), "not installed");
    }

    #[test]
    fn the_two_lists_split_at_the_mark() {
        let (all, off) = split_lists("package:b\npackage:a\n@disabled\npackage:a\n");
        assert_eq!((all, off), (names(&["a", "b"]), names(&["a"])));
    }
}
