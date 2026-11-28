//! What the examples share: picking a device from the command line and moving its screen.
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use sim_core::geometry::Point;
use sim_core::gesture::Phase;
use sim_core::stream::Input;

/// The device to encode.
pub enum Device {
    Ios { udid: String },
    Android { name: String, dir: PathBuf },
}

impl Device {
    /// `--udid <udid>` (a booted iOS device) or `--avd <name>` (a running AVD).
    pub fn from_args(args: &[String]) -> Result<Device, String> {
        if let Some(udid) = value(args, "--udid") {
            return Ok(Device::Ios { udid });
        }
        if let Some(name) = value(args, "--avd") {
            let avd = emu_avd::Env::from_env().load(&name).map_err(|e| format!("--avd {name}: {e}"))?;
            return Ok(Device::Android { name, dir: avd.dir });
        }
        Err("pass --udid <booted iOS device> or --avd <running AVD>".into())
    }

    pub fn label(&self) -> String {
        match self {
            Device::Ios { udid } => format!("ios:{udid}"),
            Device::Android { name, .. } => format!("android:{name}"),
        }
    }
}

/// `--name value`.
pub fn value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

pub fn number<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> Result<T, String> {
    match value(args, name) {
        Some(v) => v.parse().map_err(|_| format!("{name} {v}: not a number")),
        None => Ok(default),
    }
}

pub fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Keeps a screen moving until dropped: horizontal swipes between home screen pages on iOS,
/// the app drawer up and down on Android (what scripts/android-jank.sh swipes), one every
/// `period`, each 300 ms long with a move every 20 ms.
pub struct Motion {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Motion {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const STEPS: u32 = 15;
const STEP: Duration = Duration::from_millis(20);

fn strokes(stop: &AtomicBool, period: Duration, mut touch: impl FnMut(Point, Phase) -> bool, a: Point, b: Point) {
    let mut forward = true;
    while !stop.load(Ordering::Acquire) {
        let (from, to) = if forward { (a, b) } else { (b, a) };
        forward = !forward;
        if !touch(from, Phase::Down) {
            return;
        }
        for i in 1..=STEPS {
            std::thread::sleep(STEP);
            touch(from.lerp(to, f64::from(i) / f64::from(STEPS)), Phase::Move);
        }
        touch(to, Phase::Up);
        std::thread::sleep(period.saturating_sub(STEP * STEPS));
    }
}

impl Motion {
    /// iOS: a HID session of its own on its own thread (a `Context` stays on one thread).
    pub fn ios(udid: &str, period: Duration) -> Motion {
        let stop = Arc::new(AtomicBool::new(false));
        let (s, udid) = (stop.clone(), udid.to_owned());
        let thread = std::thread::spawn(move || {
            let hid = sim_coresim::Context::new().and_then(|c| c.device(&udid)).and_then(|d| d.hid());
            let Ok(hid) = hid.map_err(|e| eprintln!("motion: {e}")) else { return };
            let touch = |p: Point, phase| sim_coresim::autoreleasepool(|| hid.touch(p, phase)).is_ok();
            strokes(&s, period, touch, Point { x: 0.85, y: 0.55 }, Point { x: 0.15, y: 0.55 });
        });
        Motion { stop, thread: Some(thread) }
    }

    /// Android: touch lines on a second handle of the display connection, in device pixels.
    pub fn android(mut link: std::os::unix::net::UnixStream, (w, h): (u32, u32), period: Duration) -> Motion {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let touch = |p: Point, phase| {
                let line = format!("{}\n", Input::Touch { x: p.x as f32, y: p.y as f32, phase }.to_line());
                link.write_all(line.as_bytes()).is_ok()
            };
            let (w, h) = (f64::from(w), f64::from(h));
            strokes(&s, period, touch, Point { x: w / 2.0, y: h * 0.8 }, Point { x: w / 2.0, y: h * 0.3 });
        });
        Motion { stop, thread: Some(thread) }
    }
}

/// The median and 95th percentile of `samples`, in its unit.
pub fn median_p95(samples: &mut [f64]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    samples.sort_by(f64::total_cmp);
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.95))
}
