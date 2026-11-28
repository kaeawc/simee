//! Device discovery off the UI thread. Listing iOS devices is several Objective-C calls per
//! device and listing AVDs reads every AVD's config, so a scanner thread does both (with its own
//! simulator service connection: service objects stay on the thread that made them), every two
//! seconds and whenever the window asks, and hands `poll` the result through the waker. A scan
//! that finds nothing new isn't posted, so a still window isn't woken.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::Duration;

use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::platform::Platform;

use crate::device::{self, Context};

/// What one scan found.
#[derive(Clone, Debug, Default)]
pub struct Scan {
    /// Every available device, iOS first, booted first.
    pub devices: Vec<DeviceInfo>,
    /// The device the window had selected when the scan started.
    pub selected: Option<String>,
    /// That device, resolved, when it's an AVD (its config, profile and running instance), or
    /// why it couldn't be.
    pub android: Option<Result<sim_android::Device, String>>,
    /// That AVD's guest log, newest last: the console's critical lines (quiet unless
    /// `-show-kernel`), then the logcat stream emu writes for devices booted from here
    /// (`sim_android::device_log_tail`).
    pub console_log: Vec<String>,
    /// Why there's nothing to list.
    pub error: Option<String>,
}

impl Scan {
    /// Whether `other` shows the window anything this one doesn't.
    fn same_as(&self, other: &Scan) -> bool {
        let running = |s: &Scan| s.android.as_ref().map(|a| a.as_ref().map(|d| d.running.is_some()).map_err(String::clone));
        self.devices == other.devices
            && self.selected == other.selected
            && self.console_log == other.console_log
            && self.error == other.error
            && running(self) == running(other)
    }
}

/// Scans every iOS device and AVD; resolves `selected` when it's an AVD.
fn scan(ctx: Result<&Context, &str>, android: &sim_android::Android, selected: Option<&str>) -> Scan {
    let mut out = Scan { selected: selected.map(str::to_owned), ..Scan::default() };
    match ctx {
        Ok(ctx) => match device::list(ctx) {
            Ok(l) => out.devices.extend(l),
            Err(e) => out.error = Some(e),
        },
        Err(e) => out.error = Some(e.to_owned()),
    }
    let avds = android.devices();
    // No simulator service is only worth saying when there's nothing else to show.
    if ctx.is_err() && !avds.is_empty() {
        out.error = None;
    }
    out.devices.extend(avds.into_iter().filter(|d| d.available));
    out.devices.sort_by_key(|d| (d.platform != Platform::Ios, d.state != DeviceState::Booted, d.runtime_id.clone(), d.name.clone()));
    if let Some(udid) = selected
        && out.devices.iter().any(|d| d.udid == udid && d.platform == Platform::Android)
    {
        let resolved = android.device(udid);
        if let Some(dir) = resolved.as_ref().ok().and_then(|d| d.avd.as_ref()).map(|a| a.dir.clone()) {
            out.console_log = sim_android::device_log_tail(&dir, 200);
        }
        out.android = Some(resolved);
    }
    out
}

struct Shared {
    selected: Mutex<Option<String>>,
    asked: AtomicBool,
    stop: AtomicBool,
    latest: Mutex<Option<Scan>>,
}

/// The scanner thread's handle. Dropping it stops the thread after its current scan.
pub struct Scanner {
    shared: Arc<Shared>,
    thread: Thread,
}

impl Scanner {
    /// Scans every `every` and on [`Scanner::ask`]; `waker` wakes the window for each posted scan.
    pub fn start(every: Duration, waker: rune::Waker) -> Result<Scanner, String> {
        Scanner::start_with(every, waker, || {
            let ctx = Context::new().map_err(|e| e.to_string());
            let android = sim_android::Android::from_env();
            move |selected: Option<&str>| scan(ctx.as_ref().map_err(String::as_str), &android, selected)
        })
    }

    /// [`Scanner::start`] with the scan made by `make`, on the scanner thread.
    fn start_with<S, F>(every: Duration, waker: rune::Waker, make: F) -> Result<Scanner, String>
    where
        S: FnMut(Option<&str>) -> Scan,
        F: FnOnce() -> S + Send + 'static,
    {
        let shared = Arc::new(Shared {
            selected: Mutex::new(None),
            asked: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            latest: Mutex::new(None),
        });
        let s = shared.clone();
        let handle = std::thread::Builder::new()
            .name("simee-scan".into())
            .spawn(move || {
                let mut scan = make();
                let mut last: Option<Scan> = None;
                while !s.stop.load(Ordering::Acquire) {
                    let asked = s.asked.swap(false, Ordering::AcqRel);
                    let selected = s.selected.lock().ok().and_then(|g| g.clone());
                    let found = scan(selected.as_deref());
                    if asked || last.as_ref().is_none_or(|l| !l.same_as(&found)) {
                        last = Some(found.clone());
                        if let Ok(mut slot) = s.latest.lock() {
                            *slot = Some(found);
                        }
                        waker();
                    }
                    std::thread::park_timeout(every);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Scanner { shared, thread: handle.thread().clone() })
    }

    /// Follows `selected` from the next scan on, without asking for one.
    pub fn follow(&self, selected: Option<&str>) {
        if let Ok(mut s) = self.shared.selected.lock() {
            *s = selected.map(str::to_owned);
        }
    }

    /// Asks for a scan now (posted even if nothing changed), resolving `selected`.
    pub fn ask(&self, selected: Option<&str>) {
        self.follow(selected);
        self.shared.asked.store(true, Ordering::Release);
        self.thread.unpark();
    }

    /// The newest posted scan not taken yet.
    pub fn take(&self) -> Option<Scan> {
        self.shared.latest.lock().ok().and_then(|mut l| l.take())
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(5), "{what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// What one scan of this host's devices costs: the work the UI thread did every two seconds
    /// before the scanner. Lists devices only (read-only; boots nothing).
    /// `cd desktop && cargo test --release scan_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench; reads this host's simulator set and AVDs"]
    fn scan_bench() {
        let ctx = Context::new().map_err(|e| e.to_string());
        let android = sim_android::Android::from_env();
        let mut times: Vec<Duration> = (0..9)
            .map(|_| {
                let start = Instant::now();
                let found = scan(ctx.as_ref().map_err(String::as_str), &android, None);
                assert!(found.devices.len() < 100_000);
                start.elapsed()
            })
            .collect();
        times.sort();
        let n = scan(ctx.as_ref().map_err(String::as_str), &android, None).devices.len();
        println!("scan of {n} devices: median {:?} (min {:?}, max {:?})", times[4], times[0], times[8]);
    }

    /// A selected AVD's panel gets its logcat stream: the console is empty on a normal boot.
    #[test]
    fn a_selected_avds_log_includes_its_logcat() {
        let root = std::env::temp_dir().join(format!("desktop-scan-log-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nPkg.Revision=7\nSystemImage.TagId=google_apis\n")
            .expect("props");
        let mut android = sim_android::Android::new(sim_android::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let dir = root.join("avd/phone.avd/simee");
        std::fs::create_dir_all(&dir).expect("simee");
        std::fs::write(dir.join("console.log"), "").expect("quiet console");
        std::fs::write(dir.join("logcat.txt"), "11-23 20:00:00.000  512  512 I ActivityManager: Start proc\n").expect("logcat");
        let found = scan(Err("no simulator service"), &android, Some("phone"));
        assert_eq!(found.console_log, ["11-23 20:00:00.000  512  512 I ActivityManager: Start proc"]);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Scans run on the scanner's thread; one that finds nothing new isn't posted and doesn't
    /// wake the window, an asked-for one is, and it resolves what the window selected.
    #[test]
    fn scans_run_off_the_ui_thread_and_post_only_news() {
        let (woken, scans) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let w = woken.clone();
        let n = scans.clone();
        let ui = std::thread::current().id();
        let scanner = Scanner::start_with(
            Duration::from_millis(5),
            Arc::new(move || {
                w.fetch_add(1, Ordering::Relaxed);
            }),
            move || {
                move |selected: Option<&str>| {
                    assert_ne!(std::thread::current().id(), ui);
                    n.fetch_add(1, Ordering::Relaxed);
                    Scan { selected: selected.map(str::to_owned), ..Scan::default() }
                }
            },
        )
        .expect("start");
        wait_for("first scan posted", || woken.load(Ordering::Relaxed) == 1);
        assert!(scanner.take().is_some_and(|s| s.selected.is_none()));
        // Periodic scans that find the same thing: not posted.
        let seen = scans.load(Ordering::Relaxed);
        wait_for("more scans", || scans.load(Ordering::Relaxed) >= seen + 5);
        assert_eq!(woken.load(Ordering::Relaxed), 1, "unchanged scans don't wake the window");
        assert!(scanner.take().is_none());
        scanner.ask(Some("pixel_9"));
        // At least once: a scan that read `asked` just before the ask but the selection just
        // after it posts the new selection as news, then the ask posts it again, so the count
        // can pass 2 between two looks.
        wait_for("asked scan posted", || woken.load(Ordering::Relaxed) >= 2);
        assert_eq!(scanner.take().and_then(|s| s.selected).as_deref(), Some("pixel_9"));
        drop(scanner);
        let stopped = scans.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(30));
        assert!(scans.load(Ordering::Relaxed) <= stopped + 1, "the thread stops");
    }
}
