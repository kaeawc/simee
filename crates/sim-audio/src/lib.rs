//! Per-device simulator audio, without a host audio driver.
//!
//! A simulated device's processes are ordinary clients of the host's Core Audio server, so:
//! - **Capture / mute:** a Core Audio process tap (public, macOS 14.2+) over the device's processes
//!   captures exactly that device's output, and a muted tap silences it on the host (a per-device
//!   null sink). The tap runs in a small Swift helper (`native/simaudio.swift`), built on demand.
//! - **Route / volume:** each device reads its host devices and volume from
//!   `<data>/var/run/simulatoraudio/audiosettings.plist` when an audio client starts.
//!
//! Capture needs the System Audio Recording permission for the app that runs simee.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::Value;
use sim_core::plist::Scalar;

/// The helper's Swift source.
pub const HELPER_SOURCE: &str = include_str!("../native/simaudio.swift");

/// The UID that means "follow the host's default device".
pub const HOST_DEFAULT_UID: &str = "__sim__hostUseSystemDefaultDeviceUID";

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Builds (or reuses) the helper and returns its path.
pub fn helper() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = home.join("Library/Caches/simee/audio");
    let out = dir.join(format!("simaudio-{:016x}", fnv1a(HELPER_SOURCE)));
    if out.is_file() {
        return Ok(out);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let src = dir.join("simaudio.swift");
    std::fs::write(&src, HELPER_SOURCE).map_err(|e| e.to_string())?;
    let tmp = dir.join("simaudio.building");
    let built = Command::new("/usr/bin/xcrun").args(["swiftc", "-O", "-o"]).arg(&tmp).arg(&src).output().map_err(|e| e.to_string())?;
    if !built.status.success() {
        return Err(format!("building the audio helper failed: {}", String::from_utf8_lossy(&built.stderr).trim()));
    }
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// This host's macOS release, `None` off macOS (or unreadable: the helper then says).
fn host_macos() -> Option<sim_core::support::Version> {
    let text = std::fs::read_to_string(sim_core::support::SYSTEM_VERSION_PLIST).ok()?;
    sim_core::support::Version::from_system_version(&text).map(|(v, _)| v)
}

/// Host audio devices as the helper reports them.
pub fn host_devices() -> Result<Value, String> {
    let out = Command::new(helper()?).arg("devices").output().map_err(|e| e.to_string())?;
    serde_json::from_slice(&out.stdout).map_err(|e| format!("bad helper output: {e}"))
}

/// The device's audio settings file.
pub fn settings_path(data_path: &Path) -> PathBuf {
    data_path.join("var/run/simulatoraudio/audiosettings.plist")
}

/// The settings file's entries; an error when it can't be read or isn't the flat XML dictionary
/// the runtime writes.
fn read_settings(path: &Path) -> Result<Vec<(String, Scalar)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    sim_core::plist::parse_dict(&text)
        .ok_or_else(|| format!("{}: not an XML property list of one dictionary of strings, numbers and booleans", path.display()))
}

/// Reads the device's audio settings (UIDs, volume, ringer) as JSON (read in process: no `plutil`).
pub fn settings(data_path: &Path) -> Result<Value, String> {
    let entries = read_settings(&settings_path(data_path))?;
    Ok(Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Scalar::String(s) => Value::from(s),
                    Scalar::Integer(i) => Value::from(i),
                    Scalar::Real(r) => Value::from(r),
                    Scalar::Bool(b) => Value::from(b),
                };
                (k, v)
            })
            .collect(),
    ))
}

/// Sets `key` in the device's settings file, rewritten in process (to a temporary file renamed
/// over it, so a reader never sees half a file).
fn replace(data_path: &Path, key: &str, value: Scalar) -> Result<(), String> {
    let path = settings_path(data_path);
    if !path.exists() {
        return Err(format!("{} does not exist; is the device booted?", path.display()));
    }
    let mut entries = read_settings(&path)?;
    sim_core::plist::set(&mut entries, key, value);
    // Unique per process and call, so two writers never share a temporary file (the last rename
    // wins whole).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("plist.simee-{}-{n}", std::process::id()));
    std::fs::write(&tmp, sim_core::plist::render_dict(&entries)).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Sets the device's output volume, 0–100 (100 is unity gain). Applies to audio clients started afterwards.
pub fn set_volume(data_path: &Path, level: u8) -> Result<(), String> {
    replace(data_path, "sim_volume", Scalar::Integer(i64::from(level.min(100))))
}

/// Pins the device's input or output to a host device UID (or `default`). Unlike the simulator's
/// router, this accepts any device (including loopback devices the router hides). Applies to audio
/// clients started afterwards; the simulator's audio service rewrites the file on boot and when host
/// devices change, so callers re-apply after those.
pub fn set_route(data_path: &Path, output: bool, uid: &str) -> Result<(), String> {
    let uid = if uid == "default" { HOST_DEFAULT_UID } else { uid };
    replace(data_path, if output { "sim_output_device_uid" } else { "sim_input_device_uid" }, Scalar::String(uid.to_owned()))
}

/// A running tap of one device's audio output.
pub struct Capture {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    pub started: Value,
    pub path: Option<PathBuf>,
}

impl Capture {
    /// Starts tapping the device whose `launchd_sim` is `launchd_pid`. With `mute`, the device is
    /// silent on the host meanwhile. With `out`, samples are written there as WAV on stop. Waits
    /// until the device has an audio client (up to 30 s).
    pub fn start(launchd_pid: i32, out: Option<&Path>, mute: bool) -> Result<Self, String> {
        // Below the floor the helper would answer the same, after a build that may not even
        // compile against an older SDK.
        sim_core::support::check("audio_capture", host_macos())?;
        let mut cmd = Command::new(helper()?);
        cmd.args(["tap", "--launchd", &launchd_pid.to_string()]);
        if let Some(p) = out {
            cmd.arg("--out").arg(p);
        }
        if mute {
            cmd.arg("--mute");
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().ok_or("no helper stdout")?);
        let mut line = String::new();
        stdout.read_line(&mut line).map_err(|e| e.to_string())?;
        let started: Value = serde_json::from_str(&line).map_err(|_| format!("audio helper failed to start: {}", line.trim()))?;
        if let Some(e) = started.get("error").and_then(Value::as_str) {
            let _ = child.wait();
            return Err(e.to_owned());
        }
        Ok(Self { child, stdin, stdout, started, path: out.map(Path::to_path_buf) })
    }

    /// Stops the tap and returns its stats (frames, duration_ms, rms, peak, dbfs, path).
    pub fn stop(mut self) -> Result<Value, String> {
        drop(self.stdin.take()); // EOF tells the helper to stop
        let mut last = String::new();
        for line in (&mut self.stdout).lines().map_while(Result::ok) {
            last = line;
        }
        let _ = self.child.wait();
        serde_json::from_str(&last).map_err(|_| format!("audio helper ended without stats: {last}"))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.flush();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_path_and_default_uid() {
        assert_eq!(settings_path(Path::new("/d")), PathBuf::from("/d/var/run/simulatoraudio/audiosettings.plist"));
        assert!(HOST_DEFAULT_UID.starts_with("__sim__"));
        assert_ne!(fnv1a(HELPER_SOURCE), fnv1a(""));
    }

    // Settings are read and edited in process, so this runs on any host.
    #[test]
    fn route_and_volume_edit_a_settings_file() {
        let root = std::env::temp_dir().join(format!("sim-audio-test-{}", std::process::id()));
        let path = settings_path(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>sim_volume</key><integer>100</integer></dict></plist>"#).expect("plist");
        set_volume(&root, 40).expect("volume");
        set_route(&root, true, "default").expect("route");
        let s = settings(&root).expect("read");
        assert_eq!(s, serde_json::json!({"sim_volume": 40, "sim_output_device_uid": HOST_DEFAULT_UID}));
        let left: Vec<_> = std::fs::read_dir(path.parent().expect("parent")).expect("dir").flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("audiosettings.plist")], "no temporary file left");
        std::fs::remove_dir_all(root).ok();
    }

    /// What `plutil -convert json` printed for the runtime's file, and what `plutil -replace` left
    /// after `sim_volume` 40 (iOS 27.2, macOS 26.6), against the in-process read and write.
    #[test]
    fn reads_and_writes_match_what_plutil_did_on_the_runtimes_file() {
        let root = std::env::temp_dir().join(format!("sim-audio-plutil-{}", std::process::id()));
        let path = settings_path(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, include_str!("../fixtures/audiosettings.plist")).expect("plist");
        let plutil_json: Value = serde_json::from_str(include_str!("../fixtures/audiosettings.plutil.json")).expect("json");
        assert_eq!(settings(&root).expect("read"), plutil_json);
        set_volume(&root, 40).expect("volume");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), include_str!("../fixtures/audiosettings.plutil-replaced.plist"));
        std::fs::remove_dir_all(root).ok();
    }

    /// The tap's floor is the support matrix's, and both the helper's guard and its fallback
    /// message name it; this host (macOS 14.2+) passes the check.
    #[test]
    fn the_tap_is_guarded_at_its_macos_floor() {
        let floor = sim_core::support::floor("audio_capture").and_then(|f| f.macos).expect("floor");
        assert_eq!(floor.to_string(), "14.2");
        assert!(HELPER_SOURCE.contains("@available(macOS 14.2, *)\nfunc tap("), "tap is annotated");
        assert!(HELPER_SOURCE.contains("if #available(macOS 14.2, *) {\n        tap("), "its call is guarded");
        assert!(HELPER_SOURCE.contains("audio capture needs macOS 14.2 or later"));
        if cfg!(target_os = "macos") {
            let v = host_macos().expect("SystemVersion.plist");
            assert_eq!(sim_core::support::check("audio_capture", Some(v)).is_ok(), v >= floor);
        }
    }

    #[test]
    fn a_missing_or_unreadable_file_is_an_error() {
        let root = std::env::temp_dir().join(format!("sim-audio-bad-{}", std::process::id()));
        assert!(set_volume(&root, 1).expect_err("missing").contains("does not exist"));
        let path = settings_path(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, "bplist00").expect("write");
        assert!(set_volume(&root, 1).expect_err("binary").contains("not an XML property list"));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "bplist00", "left as it was");
        assert!(settings(&root).is_err());
        std::fs::remove_dir_all(root).ok();
    }
}
