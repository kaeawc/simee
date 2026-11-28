//! Device state on Android, in the shapes of the iOS methods of the same names (#418): erasing a
//! shut-down AVD's user data, and over adb on a running one SystemUI's demo-mode status bar, the
//! contrast settings, the on-screen keyboard beside the emulator's hardware one, and a
//! trim-memory signal as the memory warning.
//!
//! Every shell line here is built from fixed words and numbers, or a checked package name: no
//! caller text reaches the guest's shell.

use std::fs::File;

use crate::apps::{Adb, package_name};
use crate::{Android, Device};

/// The broadcast SystemUI's demo mode listens for, up to the command's name.
const DEMO: &str = "am broadcast -a com.android.systemui.demo -e command";

/// The first API level with the system color contrast setting (`Settings.Secure.CONTRAST_LEVEL`,
/// `UiModeManager.getContrast`).
pub const CONTRAST_LEVEL_API: u32 = 34;

/// A Wi-Fi or cellular indicator in demo mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Radio {
    /// Not shown.
    Hidden,
    /// Shown with no bars and not connected (iOS's searching and failed).
    Down,
    /// Connected, with this many bars (0-4).
    Up(u8),
}

/// The demo-mode status bar: each item `None` is left as demo mode shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DemoStatusBar {
    /// The clock, `HHMM` ([`demo_clock`]).
    pub clock: Option<String>,
    pub wifi: Option<Radio>,
    pub mobile: Option<Radio>,
    /// The mobile data type demo mode names (`lte`, `5g+`; `none` shows none).
    pub datatype: Option<&'static str>,
    /// Level (0-100) and whether it is plugged in.
    pub battery: Option<(u8, bool)>,
}

impl DemoStatusBar {
    /// The shell commands that show it, in order: demo mode allowed and entered, then one
    /// broadcast per item (Wi-Fi and mobile apart: both read the same `level` extra).
    pub fn commands(&self) -> Vec<String> {
        let mut out = vec!["settings put global sysui_demo_allowed 1".to_owned(), format!("{DEMO} enter")];
        if let Some(hhmm) = &self.clock {
            out.push(format!("{DEMO} clock -e hhmm {hhmm}"));
        }
        if let Some(wifi) = self.wifi {
            out.push(format!("{DEMO} network -e wifi {}", radio(wifi)));
        }
        if self.mobile.is_some() || self.datatype.is_some() {
            let mut mobile = format!("{DEMO} network -e mobile {}", self.mobile.map_or("show".to_owned(), radio));
            if let Some(datatype) = self.datatype {
                mobile.push_str(&format!(" -e datatype {datatype}"));
            }
            out.push(mobile);
        }
        if let Some((level, plugged)) = self.battery {
            out.push(format!("{DEMO} battery -e level {} -e plugged {plugged}", level.min(100)));
        }
        out
    }
}

/// A [`Radio`] as the `network` command's extras after `wifi` or `mobile`.
fn radio(r: Radio) -> String {
    match r {
        Radio::Hidden => "hide".to_owned(),
        Radio::Down => "show -e level 0 -e fully false".to_owned(),
        Radio::Up(level) => format!("show -e level {} -e fully true", level.min(4)),
    }
}

/// `time` as demo mode's clock (`HHMM`): `9:41`, `09:41`, `0941`, or an ISO 8601 date and time
/// (its time of day). None when it holds no time of day.
pub fn demo_clock(time: &str) -> Option<String> {
    let time = time.split_once('T').map_or(time, |(_, t)| t);
    let (h, m) = match time.split_once(':') {
        Some((h, rest)) => (h, rest.get(..2)?),
        None if time.len() == 4 => time.split_at_checked(2)?,
        None => return None,
    };
    let number = |s: &str, below: u8| -> Option<u8> {
        let digits = !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
        digits.then(|| s.parse().ok()).flatten().filter(|n| *n < below)
    };
    Some(format!("{:02}{:02}", number(h, 24)?, number(m, 60)?))
}

/// A shell's output as an error when a command in it said it failed (`am` prints an exception,
/// `settings` an error, and the shell may still exit 0).
fn said_failed(what: &str, out: &str) -> Result<(), String> {
    match out.lines().map(str::trim).find(|l| l.starts_with("Error") || l.starts_with("Exception") || l.contains("Exception:")) {
        Some(line) => Err(format!("{what}: {line}")),
        None => Ok(()),
    }
}

/// The package of the first resumed activity in `dumpsys activity activities`' lines
/// (`topResumedActivity=ActivityRecord{6d5b2c1 u0 com.example.app/.MainActivity t12}`; before
/// API 29, `mResumedActivity: ...`).
fn foreground_package(out: &str) -> Option<String> {
    out.lines()
        .filter(|l| l.contains("topResumedActivity=") || l.contains("mResumedActivity:"))
        .find_map(|l| l.split_whitespace().find_map(|w| w.split_once('/').map(|(p, _)| p)).and_then(|p| package_name(p).ok()))
        .map(str::to_owned)
}

impl Adb {
    /// Shows `bar` through SystemUI's demo mode, in one shell.
    pub fn demo_status_bar(&self, bar: &DemoStatusBar) -> Result<(), String> {
        let out = self.shell(&bar.commands().join(" && "), self.deadlines.query)?;
        said_failed("status bar", &out)
    }

    /// Leaves demo mode: the real status bar is back.
    pub fn clear_demo_status_bar(&self) -> Result<(), String> {
        let out = self.shell(&format!("{DEMO} exit"), self.deadlines.query)?;
        said_failed("clear status bar", &out)
    }

    /// High-contrast text on or off, and on a guest of API `api` 34 or later the system color
    /// contrast too (high or standard).
    pub fn set_increase_contrast(&self, on: bool, api: Option<u32>) -> Result<(), String> {
        let mut command = format!("settings put secure high_text_contrast_enabled {}", u8::from(on));
        if api.is_some_and(|a| a >= CONTRAST_LEVEL_API) {
            command.push_str(&format!(" && settings put secure contrast_level {}", if on { "1.0" } else { "0.0" }));
        }
        let out = self.shell(&command, self.deadlines.query)?;
        said_failed("increase contrast", &out)
    }

    /// The emulator's keyboard device is always attached; `connected` false shows the on-screen
    /// keyboard beside it (`show_ime_with_hard_keyboard`), true hides it, as a connected hardware
    /// keyboard does.
    pub fn set_hardware_keyboard(&self, connected: bool) -> Result<(), String> {
        let command = format!("settings put secure show_ime_with_hard_keyboard {}", u8::from(!connected));
        let out = self.shell(&command, self.deadlines.query)?;
        said_failed("hardware keyboard", &out)
    }

    /// Sends `RUNNING_CRITICAL`, the trim level of a running app when the system is about to
    /// kill it for memory, to `package`'s process, by default the foreground app's. Returns the
    /// package it went to.
    pub fn memory_warning(&self, package: Option<&str>) -> Result<String, String> {
        let package = match package {
            Some(p) => package_name(p)?.to_owned(),
            None => {
                // `|| true`: grep exits 1 when nothing is resumed (a guest still booting).
                let lines = "dumpsys activity activities | grep -E 'topResumedActivity=|mResumedActivity:' || true";
                let out = self.shell(lines, self.deadlines.query)?;
                foreground_package(&out).ok_or("no app is in the foreground; give `bundle_id`")?
            }
        };
        let out = self.shell(&format!("am send-trim-memory {package} RUNNING_CRITICAL"), self.deadlines.query)?;
        said_failed("memory warning", &out)?;
        Ok(package)
    }
}

impl Android {
    /// Erases a shut-down AVD's user data: simee's disks in `<avd>/simee/` (user data, cache and
    /// the metadata partition with user data's key) are removed and, when the AVD's image has a
    /// golden of the AVD's shape, cloned again from it, so the next boot is warm. Without one, the
    /// next boot formats user data, as a new AVD's first does. The AVD keeps its name, config and
    /// `boot.prop`; the stock emulator's own files are left alone. Refused while it runs. Returns
    /// the golden it was restored from, if any.
    pub fn erase(&self, device: &Device) -> Result<Option<String>, String> {
        let name = &device.info.name;
        if let Some(r) = &device.running {
            return Err(format!("{name} is running ({}, PID {}); shut it down before erasing it", r.serial(), r.pid));
        }
        let avd = device.avd()?;
        let disks = emu_avd::state_dir(&avd.dir);
        let io = |p: &std::path::Path, e: std::io::Error| format!("{}: {e}", p.display());
        std::fs::create_dir_all(&disks).map_err(|e| io(&disks, e))?;
        // Held for the whole erase: emu takes it to boot, so the AVD can't start halfway through.
        let lock_path = disks.join("instance.lock");
        let lock = File::create(&lock_path).map_err(|e| io(&lock_path, e))?;
        if lock.try_lock().is_err() {
            return Err(format!("{name} is running (its instance lock is held); shut it down before erasing it"));
        }
        for f in emu_avd::CLONED_DISKS {
            for p in [disks.join(f), disks.join(format!(".{f}.partial"))] {
                match std::fs::remove_file(&p) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(&p, e)),
                    _ => {}
                }
            }
        }
        // An image whose identity can't be read (a missing file) has no golden: a first boot.
        let Some(golden) = self.env.golden_key(avd).ok().and_then(|key| self.env.find_golden(&key)) else {
            return Ok(None);
        };
        for (f, _) in &golden.disks {
            if let Err(e) = emu_avd::golden::clone_file_atomic(&golden.dir.join(f), &disks.join(f)) {
                // Half a golden is neither state: leave none of it, so the next boot is a first one.
                for f in emu_avd::CLONED_DISKS {
                    let _ = std::fs::remove_file(disks.join(f));
                }
                return Err(format!("restoring {name}'s user data from golden {}: {e}", golden.key.name));
            }
        }
        drop(lock);
        Ok(Some(golden.key.name))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator, write_script};

    /// A guest's adb for the state commands, logged like [`fake_adb`]'s: `settings put` and the
    /// demo broadcasts answer as an API 36 guest does, the foreground app is `com.example.app`
    /// unless `nofg` exists beside the script, and `am send-trim-memory` knows only the processes
    /// `com.example.app` and `com.android.systemui`.
    const STATE_ADB: &str = r#"echo "$*" >> "$(dirname "$0")/adb.log"
[ "$1" = "-s" ] && shift 2
case "$2" in
  "dumpsys activity activities | grep -E 'topResumedActivity=|mResumedActivity:' || true")
    [ -e "$(dirname "$0")/nofg" ] || echo '  topResumedActivity=ActivityRecord{6d5b2c1 u0 com.example.app/.MainActivity t12}';;
  "am send-trim-memory com.example.app RUNNING_CRITICAL"|"am send-trim-memory com.android.systemui RUNNING_CRITICAL") ;;
  "am send-trim-memory "*)
    proc="${2#am send-trim-memory }"
    echo "Exception occurred while executing 'send-trim-memory':" >&2
    echo "java.lang.IllegalArgumentException: Unknown process: ${proc%% *}" >&2; exit 255;;
  *"settings put secure contrast_level"*"bad"*) echo "Error: bad value" >&2; exit 1;;
  "settings put "*|"am broadcast -a com.android.systemui.demo "*)
    case "$2" in *"am broadcast"*) printf 'Broadcasting: Intent { act=com.android.systemui.demo flg=0x400000 (has extras) }\nBroadcast completed: result=0\n';; esac;;
  *) echo "fake adb: unhandled: $2" >&2; exit 2;;
esac
"#;

    struct Fixture {
        root: PathBuf,
        android: Android,
        calls: FakeAdb,
        _fake: Option<FakeEmulator>,
    }

    impl Fixture {
        fn adb(&self) -> Adb {
            self.android.device("phone").expect("device").adb(&self.android).expect("adb")
        }

        fn shells(&self) -> Vec<String> {
            self.calls.calls().into_iter().map(|c| c.strip_prefix("shell ").map_or(c.clone(), str::to_owned)).collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// AVD `phone` on an API `api` image, running (a fake emulator, the state adb) or not.
    fn fixture(tag: &str, api: u32, running: bool) -> Fixture {
        let root = PathBuf::from("/tmp").join(format!("sa-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let image = root.join(format!("sdk/system-images/android-{api}/google_apis/arm64-v8a"));
        std::fs::create_dir_all(&image).expect("image dir");
        for f in emu_avd::bundle::IMAGE_FILES {
            std::fs::write(image.join(f), f).expect("image file");
        }
        std::fs::write(
            image.join("source.properties"),
            format!("Pkg.Revision=1\nAndroidVersion.ApiLevel={api}\nSystemImage.Abi=arm64-v8a\nSystemImage.TagId=google_apis\n"),
        )
        .expect("props");
        let mut android = Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", &format!("system-images;android-{api};google_apis;arm64-v8a"), None).expect("create");
        let calls = fake_adb(&android);
        write_script(&root.join("sdk/platform-tools/adb"), STATE_ADB);
        let _fake = running.then(|| fake_emulator(&android, "phone"));
        Fixture { root, android, calls, _fake }
    }

    #[test]
    fn the_clock_takes_a_time_of_day() {
        assert_eq!(demo_clock("9:41").as_deref(), Some("0941"));
        assert_eq!(demo_clock("09:41").as_deref(), Some("0941"));
        assert_eq!(demo_clock("23:05").as_deref(), Some("2305"));
        assert_eq!(demo_clock("0941").as_deref(), Some("0941"));
        assert_eq!(demo_clock("2007-01-09T09:41:00Z").as_deref(), Some("0941"));
        assert_eq!(demo_clock("2007-01-09T21:41:00.000-08:00").as_deref(), Some("2141"));
        for bad in ["", "noon", "9", "24:00", "12:60", "12:5", "941", "-1:30", "1:2x"] {
            assert_eq!(demo_clock(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_status_bar_is_one_broadcast_per_item() {
        let all = DemoStatusBar {
            clock: Some("0941".into()),
            wifi: Some(Radio::Up(4)),
            mobile: Some(Radio::Up(3)),
            datatype: Some("lte"),
            battery: Some((80, true)),
        };
        assert_eq!(
            all.commands(),
            [
                "settings put global sysui_demo_allowed 1",
                "am broadcast -a com.android.systemui.demo -e command enter",
                "am broadcast -a com.android.systemui.demo -e command clock -e hhmm 0941",
                "am broadcast -a com.android.systemui.demo -e command network -e wifi show -e level 4 -e fully true",
                "am broadcast -a com.android.systemui.demo -e command network -e mobile show -e level 3 -e fully true -e datatype lte",
                "am broadcast -a com.android.systemui.demo -e command battery -e level 80 -e plugged true",
            ]
        );
        let down = DemoStatusBar { wifi: Some(Radio::Hidden), mobile: Some(Radio::Down), battery: Some((5, false)), ..Default::default() };
        assert_eq!(
            down.commands()[2..],
            [
                "am broadcast -a com.android.systemui.demo -e command network -e wifi hide",
                "am broadcast -a com.android.systemui.demo -e command network -e mobile show -e level 0 -e fully false",
                "am broadcast -a com.android.systemui.demo -e command battery -e level 5 -e plugged false",
            ]
        );
        // A data type alone shows the mobile indicator with it and leaves its bars.
        let datatype = DemoStatusBar { datatype: Some("5g+"), ..Default::default() };
        assert_eq!(
            datatype.commands()[2..],
            ["am broadcast -a com.android.systemui.demo -e command network -e mobile show -e datatype 5g+"]
        );
        // Nothing given: demo mode entered, nothing else.
        assert_eq!(DemoStatusBar::default().commands().len(), 2);
    }

    #[test]
    fn the_status_bar_runs_in_one_shell_and_clears_by_leaving_demo_mode() {
        let f = fixture("bar", 36, true);
        let bar = DemoStatusBar { clock: Some("0941".into()), battery: Some((100, true)), ..Default::default() };
        assert_eq!(f.adb().demo_status_bar(&bar), Ok(()));
        assert_eq!(f.adb().clear_demo_status_bar(), Ok(()));
        assert_eq!(f.shells(), [bar.commands().join(" && "), "am broadcast -a com.android.systemui.demo -e command exit".to_owned(),]);
    }

    #[test]
    fn contrast_sets_high_text_contrast_and_from_api_34_the_color_contrast() {
        let f = fixture("contrast", 36, true);
        let adb = f.adb();
        assert_eq!(adb.set_increase_contrast(true, Some(34)), Ok(()));
        assert_eq!(adb.set_increase_contrast(false, Some(36)), Ok(()));
        assert_eq!(adb.set_increase_contrast(true, Some(33)), Ok(()));
        assert_eq!(adb.set_increase_contrast(false, None), Ok(()));
        assert_eq!(
            f.shells(),
            [
                "settings put secure high_text_contrast_enabled 1 && settings put secure contrast_level 1.0",
                "settings put secure high_text_contrast_enabled 0 && settings put secure contrast_level 0.0",
                "settings put secure high_text_contrast_enabled 1",
                "settings put secure high_text_contrast_enabled 0",
            ]
        );
    }

    #[test]
    fn the_hardware_keyboard_hides_or_shows_the_on_screen_one() {
        let f = fixture("kbd", 36, true);
        assert_eq!(f.adb().set_hardware_keyboard(true), Ok(()));
        assert_eq!(f.adb().set_hardware_keyboard(false), Ok(()));
        assert_eq!(f.shells(), ["settings put secure show_ime_with_hard_keyboard 0", "settings put secure show_ime_with_hard_keyboard 1"]);
    }

    #[test]
    fn the_memory_warning_goes_to_the_foreground_app_or_the_one_named() {
        let f = fixture("trim", 36, true);
        let adb = f.adb();
        assert_eq!(adb.memory_warning(None).as_deref(), Ok("com.example.app"));
        assert_eq!(adb.memory_warning(Some("com.android.systemui")).as_deref(), Ok("com.android.systemui"));
        assert_eq!(
            f.shells(),
            [
                "dumpsys activity activities | grep -E 'topResumedActivity=|mResumedActivity:' || true",
                "am send-trim-memory com.example.app RUNNING_CRITICAL",
                "am send-trim-memory com.android.systemui RUNNING_CRITICAL",
            ]
        );
        let unknown = adb.memory_warning(Some("com.example.gone")).expect_err("no such process");
        assert!(unknown.ends_with("Unknown process: com.example.gone"), "{unknown}");
        let bad = adb.memory_warning(Some("com.example;reboot")).expect_err("not a package");
        assert!(bad.contains("isn't a package name"), "{bad}");
        std::fs::write(f.root.join("sdk/platform-tools/nofg"), "").expect("no foreground");
        let none = adb.memory_warning(None).expect_err("nothing in front");
        assert_eq!(none, "no app is in the foreground; give `bundle_id`");
        assert_eq!(f.shells().len(), 5, "the bad name never reached adb");
    }

    /// What a booted guest leaves in `dir/simee/`.
    fn settle(dir: &Path, marker: &str) {
        let d = emu_avd::state_dir(dir);
        std::fs::create_dir_all(&d).expect("simee");
        for f in emu_avd::CLONED_DISKS {
            std::fs::write(d.join(f), format!("{marker} {f}")).expect("disk");
        }
        std::fs::write(d.join("console.log"), "log").expect("log");
    }

    fn disk(dir: &Path, f: &str) -> Option<String> {
        std::fs::read_to_string(emu_avd::state_dir(dir).join(f)).ok()
    }

    #[test]
    fn erase_restores_the_golden_and_keeps_the_avd() {
        let f = fixture("golden", 36, false);
        // The image's golden, settled from another AVD of the same shape.
        f.android.create("first", "system-images;android-36;google_apis;arm64-v8a", None).expect("first");
        let built = f.android.env.ensure_golden(
            "first",
            |avd| {
                settle(&avd.dir, "settled");
                Ok(())
            },
            &mut |_| {},
        );
        let golden = built.expect("golden").key.name;
        let device = f.android.device("phone").expect("phone");
        let dir = device.avd.as_ref().expect("avd").dir.clone();
        settle(&dir, "used");
        std::fs::write(dir.join("userdata-qemu.img"), "stock's").expect("stock's own");
        let config = std::fs::read_to_string(dir.join("config.ini")).expect("config");
        assert_eq!(f.android.erase(&device), Ok(Some(golden)));
        for d in emu_avd::CLONED_DISKS {
            assert_eq!(disk(&dir, d), Some(format!("settled {d}")), "{d} is the golden's");
        }
        assert_eq!(std::fs::read_to_string(dir.join("config.ini")).expect("config"), config, "its config is kept");
        assert_eq!(std::fs::read_to_string(dir.join("userdata-qemu.img")).expect("stock"), "stock's", "stock's files are left alone");
        assert_eq!(disk(&dir, "console.log").as_deref(), Some("log"), "only the disks go");
        assert!(f.android.devices().iter().any(|d| d.udid == "phone"), "it is still listed");
    }

    #[test]
    fn erase_without_a_golden_leaves_a_first_boot() {
        let f = fixture("fresh", 36, false);
        let device = f.android.device("phone").expect("phone");
        let dir = device.avd.as_ref().expect("avd").dir.clone();
        settle(&dir, "used");
        assert_eq!(f.android.erase(&device), Ok(None));
        for d in emu_avd::CLONED_DISKS {
            assert_eq!(disk(&dir, d), None, "{d} is gone");
        }
        // Never booted: nothing to remove, and no error.
        f.android.create("new", "system-images;android-36;google_apis;arm64-v8a", None).expect("new");
        assert_eq!(f.android.erase(&f.android.device("new").expect("new")), Ok(None));
    }

    #[test]
    fn erase_refuses_a_running_device() {
        let f = fixture("running", 36, true);
        let device = f.android.device("phone").expect("phone");
        let dir = device.avd.as_ref().expect("avd").dir.clone();
        settle(&dir, "used");
        let err = f.android.erase(&device).expect_err("running");
        assert!(err.starts_with("phone is running (emulator-"), "{err}");
        assert!(err.ends_with("shut it down before erasing it"), "{err}");
        assert_eq!(disk(&dir, "userdata.img").as_deref(), Some("used userdata.img"), "nothing was removed");
        // Running but not advertised: its instance lock is held.
        let stopped = Device { running: None, ..device };
        let lock = File::create(emu_avd::state_dir(&dir).join("instance.lock")).expect("lock");
        lock.try_lock().expect("hold it");
        let err = f.android.erase(&stopped).expect_err("locked");
        assert_eq!(err, "phone is running (its instance lock is held); shut it down before erasing it");
        assert_eq!(disk(&dir, "userdata.img").as_deref(), Some("used userdata.img"), "nothing was removed");
    }
}
