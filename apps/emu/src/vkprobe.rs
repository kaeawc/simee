//! One-time check that gfxstream can bring up Vulkan on MoltenVK (and on KosmicKrisp, for the
//! guests that run on it). The renderer initializes once per process and silently disables
//! Vulkan on failure, so the check runs in a child process (`emu -probe-vulkan <lib64>`,
//! `emu -probe-kosmickrisp <lib64>`) and its verdict is cached per renderer build.
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use emu_gfxstream::Icd;

/// How long a probe child may take before it is killed and the boot goes on without its verdict.
/// A cold probe compiles in about 4 s on an idle Mac. The limit is generous so that a slow probe on a
/// loaded farm host still gets its verdict instead of sending the boot to lavapipe; it only has to
/// end a probe that hangs.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long `-warm-renderer` may take (about 4 s with Metal's shader cache cold).
const WARM_TIMEOUT: Duration = Duration::from_secs(120);

/// The exit status a probe child reports "Vulkan didn't come up" with: a verdict, unlike a crash.
pub const PROBE_FAILED: u8 = 3;

/// What a probe child said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Vulkan came up.
    Pass,
    /// The probe ran to the end and Vulkan didn't come up.
    Fail,
    /// No verdict (why: a timeout, a crash, a signal): never cached.
    Unclear(String),
}

fn cache_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches/simee"))
}

/// The verdict file for `icd` on the renderer build `lib64`, under `dir`: keyed by the backend
/// and by the driver's own library, so a driver replaced in place is probed again.
fn cache_file_in(dir: &Path, lib64: &Path, icd: Icd) -> Option<PathBuf> {
    let backend = std::fs::metadata(lib64.join("libgfxstream_backend.dylib")).ok()?;
    let driver = std::fs::metadata(lib64.join("vulkan").join(driver_library(icd))).map_or_else(|_| "none".into(), |m| stamp(&m));
    Some(dir.join(format!("{}-{}-{driver}", icd.name(), stamp(&backend))))
}

/// The library `icd`'s manifest in the renderer's `lib64/vulkan` loads.
fn driver_library(icd: Icd) -> &'static str {
    match icd {
        Icd::MoltenVk => "libMoltenVK.dylib",
        Icd::KosmicKrisp => "libvulkan_kosmickrisp.dylib",
        Icd::Lavapipe => "libvulkan_lvp.dylib",
        Icd::SwiftShader => "libvk_swiftshader.dylib",
    }
}

/// One file of a build: its size, mtime (ns) and inode. Builds installed within the same second
/// (or copied with preserved mtimes) still differ by size or inode.
fn stamp(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}.{:09}-{}-{}", meta.mtime(), meta.mtime_nsec(), meta.len(), meta.ino())
}

/// The child flag that probes `icd`.
fn probe_flag(icd: Icd) -> &'static str {
    if icd == Icd::KosmicKrisp { "-probe-kosmickrisp" } else { "-probe-vulkan" }
}

/// Whether gfxstream brings up Vulkan on MoltenVK: the cached verdict, probing (≈1 s, at most
/// [`PROBE_TIMEOUT`]) when there is none.
pub fn moltenvk(lib64: &Path) -> Verdict {
    cached(lib64, Icd::MoltenVk)
}

/// Whether gfxstream brings up Vulkan on KosmicKrisp: cached like [`moltenvk`], probing
/// (`emu -probe-kosmickrisp`) once per renderer build.
pub fn kosmickrisp(lib64: &Path) -> Verdict {
    cached(lib64, Icd::KosmicKrisp)
}

fn cached(lib64: &Path, icd: Icd) -> Verdict {
    let Ok(exe) = std::env::current_exe() else { return Verdict::Unclear("couldn't find emu's own executable".into()) };
    let cache = cache_dir().and_then(|d| cache_file_in(&d, lib64, icd));
    verdict(cache.as_deref(), probe_command(&exe, probe_flag(icd), lib64, icd, std::env::vars_os()), PROBE_TIMEOUT)
}

/// Probes `icd` now, whatever is cached, and records a clean verdict.
fn fresh(lib64: &Path, icd: Icd) -> Verdict {
    let Ok(exe) = std::env::current_exe() else { return Verdict::Unclear("couldn't find emu's own executable".into()) };
    let v = run(probe_command(&exe, probe_flag(icd), lib64, icd, std::env::vars_os()), PROBE_TIMEOUT);
    if let Some(c) = cache_dir().and_then(|d| cache_file_in(&d, lib64, icd)) {
        record(&c, &v);
    }
    v
}

/// The verdict cached in `cache`, else `probe`'s (run for at most `timeout`), recorded when it is
/// one.
fn verdict(cache: Option<&Path>, probe: Command, timeout: Duration) -> Verdict {
    if let Some(v) = cache.and_then(read_verdict) {
        return v;
    }
    let v = run(probe, timeout);
    if let Some(c) = cache {
        record(c, &v);
    }
    v
}

/// The verdict in `cache`: `ok` or `fail`, anything else (an empty file another boot is still
/// writing) is none.
fn read_verdict(cache: &Path) -> Option<Verdict> {
    match std::fs::read_to_string(cache).ok()?.trim() {
        "ok" => Some(Verdict::Pass),
        "fail" => Some(Verdict::Fail),
        _ => None,
    }
}

/// Records a clean verdict in `cache` (written beside it and renamed over it, so a reader sees
/// the old file or the new one); an unclear one is not recorded, and the next boot probes again.
fn record(cache: &Path, v: &Verdict) {
    let text = match v {
        Verdict::Pass => "ok",
        Verdict::Fail => "fail",
        Verdict::Unclear(_) => return,
    };
    let _ = cache.parent().map(std::fs::create_dir_all);
    let mut tmp = cache.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    if std::fs::write(&tmp, text).is_err() || std::fs::rename(&tmp, cache).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// An inherited variable that picks or configures a Vulkan driver, loader or layer (the loader's
/// `VK_*`, MoltenVK's `MVK_*`, Mesa's `MESA_*`, gfxstream's `ANDROID_EMU_VK_*`) or gfxstream's
/// launcher directory. A probe child gets none of them: a user's, a bench arm's or an earlier
/// [`emu_gfxstream::configure_environment`]'s would make the cached verdict another driver's.
fn driver_setting(k: &std::ffi::OsStr) -> bool {
    let k = k.to_string_lossy();
    ["VK_", "MVK_", "MESA_", "ANDROID_EMU_VK_"].iter().any(|p| k.starts_with(p)) || k == "ANDROID_EMULATOR_LAUNCHER_DIR"
}

/// `exe <flag> <lib64>` as a probe child of `icd`: the environment `inherited` without its
/// driver settings ([`driver_setting`]), plus `icd`'s own ([`emu_gfxstream::driver_environment`]).
fn probe_command(exe: &Path, flag: &str, lib64: &Path, icd: Icd, inherited: impl IntoIterator<Item = (OsString, OsString)>) -> Command {
    let mut c = Command::new(exe);
    c.arg(flag).arg(lib64).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c.env_clear().envs(inherited.into_iter().filter(|(k, _)| !driver_setting(k)));
    if let Some(emulator_dir) = lib64.parent() {
        c.envs(emu_gfxstream::driver_environment(emulator_dir, icd));
    }
    c
}

/// Runs a probe child for at most `timeout` and reads its verdict from how it exited: 0 passes,
/// [`PROBE_FAILED`] fails, and anything else (another status, a signal, the deadline, at which
/// the child is killed by its PID and reaped) is unclear.
fn run(mut probe: Command, timeout: Duration) -> Verdict {
    let mut child = match probe.spawn() {
        Ok(c) => c,
        Err(e) => return Verdict::Unclear(format!("couldn't start ({e})")),
    };
    let pid = child.id();
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            waited => {
                // Not reaped yet, so the PID is still this child's: Child::kill signals it.
                let _ = child.kill();
                let _ = child.wait();
                return Verdict::Unclear(match waited {
                    Err(e) => format!("couldn't be waited on ({e}); killed pid {pid}"),
                    _ => format!("timed out after {:.1} s; killed pid {pid}", timeout.as_secs_f64()),
                });
            }
        }
    };
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(0), _) => Verdict::Pass,
        (Some(c), _) if c == i32::from(PROBE_FAILED) => Verdict::Fail,
        (Some(c), _) => Verdict::Unclear(format!("exited with status {c}")),
        (None, Some(s)) => Verdict::Unclear(format!("was killed by signal {s}")),
        (None, None) => Verdict::Unclear("ended without a status".into()),
    }
}

/// Initializes the renderer in children the way boots do (`-provision-renderer` runs it), so the
/// Metal shaders a boot's renderer init compiles (about 4 s cold, ANGLE's shader library most of
/// it) are in emu's shader cache before the first boot: the probe's init (a GL backend, as API 31
/// to 35 guests get), then, when MoltenVK works, an API 36 guest's (no GL, Vulkan composition),
/// which compiles about another 0.9 s the probe's doesn't. Records the MoltenVK verdict too, and
/// KosmicKrisp's where the renderer ships it, so the first API 35 boot doesn't probe.
/// Returns whether Vulkan came up, whether the API 36 init ran to the end (false when Vulkan
/// didn't come up or that child failed), and how long the children took.
pub fn warm(lib64: &Path) -> (bool, bool, std::time::Duration) {
    let t = std::time::Instant::now();
    let ok = fresh(lib64, Icd::MoltenVk) == Verdict::Pass;
    let warmed = ok
        && std::env::current_exe().is_ok_and(|exe| {
            run(probe_command(&exe, "-warm-renderer", lib64, Icd::MoltenVk, std::env::vars_os()), WARM_TIMEOUT) == Verdict::Pass
        });
    if ok && Icd::KosmicKrisp.ships_in(lib64) {
        fresh(lib64, Icd::KosmicKrisp);
    }
    (ok, warmed, t.elapsed())
}

/// Child side of [`warm`]: the renderer's init exactly as an API 36 boot makes it, host ANGLE
/// included (`instance::host_angle`), on the driver `ANDROID_EMU_VK_ICD` names (MoltenVK by
/// default: [`warm`] sets it, and scripts/bench/renderers.sh names each arm's).
pub fn warm_child(lib64: &Path) -> bool {
    let Some(emulator_dir) = lib64.parent() else { return false };
    emu_gfxstream::link_beside_exe(lib64);
    if emu_vmm::runs_without_gl(true, lib64) {
        emu_gfxstream::prefer_angle_platform("swiftshader");
    }
    let icd = std::env::var("ANDROID_EMU_VK_ICD").ok().and_then(|n| Icd::from_name(&n)).unwrap_or(Icd::MoltenVk);
    emu_gfxstream::configure_environment(emulator_dir, icd);
    emu_vmm::warm_renderer(lib64.to_path_buf(), true, true).is_ok()
}

/// Child side: init the renderer on MoltenVK and report whether Vulkan came up.
pub fn probe(lib64: &Path) -> bool {
    probe_on(lib64, Icd::MoltenVk)
}

/// Child side of [`kosmickrisp`]: [`probe`] on KosmicKrisp.
pub fn probe_kosmickrisp(lib64: &Path) -> bool {
    probe_on(lib64, Icd::KosmicKrisp)
}

fn probe_on(lib64: &Path, icd: Icd) -> bool {
    let Some(emulator_dir) = lib64.parent() else { return false };
    emu_gfxstream::configure_environment(emulator_dir, icd);
    let Ok(r) = emu_gfxstream::Renderer::load(lib64) else { return false };
    let flags = emu_gfxstream::flags::USE_GLES | emu_gfxstream::flags::USE_VK;
    r.init(320, 240, flags, false, std::sync::Arc::new(|_| {})).is_ok() && emu_gfxstream::vulkan_ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A scratch directory of its own per test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("emu-vkprobe-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            Scratch(dir)
        }

        /// A fake renderer build: the backend and every driver's manifest and library.
        fn lib64(&self) -> PathBuf {
            let lib64 = self.0.join("emulator/lib64");
            std::fs::create_dir_all(lib64.join("vulkan")).expect("vulkan dir");
            std::fs::write(lib64.join("libgfxstream_backend.dylib"), b"backend").expect("backend");
            for icd in [Icd::MoltenVk, Icd::KosmicKrisp] {
                std::fs::write(lib64.join("vulkan").join(icd.manifest()), b"{}").expect("manifest");
            }
            std::fs::write(lib64.join("vulkan/libMoltenVK.dylib"), b"moltenvk").expect("moltenvk");
            std::fs::write(lib64.join("vulkan/libvulkan_kosmickrisp.dylib"), b"kosmickrisp").expect("kosmickrisp");
            lib64
        }

        /// A fake probe child: a shell script running `body`.
        fn probe(&self, name: &str, body: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).expect("script");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn env(vars: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        vars.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))).collect()
    }

    /// The fake `probe` as a KosmicKrisp probe child of `lib64`, inheriting only a PATH.
    fn command(probe: &Path, lib64: &Path) -> Command {
        probe_command(probe, "-probe-kosmickrisp", lib64, Icd::KosmicKrisp, env(&[("PATH", "/usr/bin:/bin")]))
    }

    #[test]
    fn a_probe_that_hangs_is_killed_at_its_deadline_and_not_cached() {
        let s = Scratch::new("hang");
        let lib64 = s.lib64();
        let pid_file = s.0.join("pid");
        let hang = s.probe("hang", &format!("echo $$ > {}\nexec /bin/sleep 10", pid_file.display()));
        let cache = s.0.join("cache/kosmickrisp");
        let t = std::time::Instant::now();
        let v = verdict(Some(&cache), command(&hang, &lib64), Duration::from_millis(300));
        assert!(t.elapsed() < Duration::from_secs(5), "waited {:?} for a hung probe", t.elapsed());
        assert!(matches!(&v, Verdict::Unclear(why) if why.contains("timed out")), "{v:?}");
        assert!(!cache.exists(), "a timeout was cached: {:?}", std::fs::read_to_string(&cache));
        // Killed by the PID it was started with, and reaped.
        let pid = std::fs::read_to_string(&pid_file).expect("pid").trim().to_string();
        let alive = Command::new("/bin/ps").args(["-p", &pid]).stdout(Stdio::null()).status().expect("ps").success();
        assert!(!alive, "probe child {pid} still runs");
    }

    #[test]
    fn a_probe_that_crashes_is_not_cached() {
        let s = Scratch::new("crash");
        let lib64 = s.lib64();
        let cache = s.0.join("cache/kosmickrisp");
        // A fault (SIGSEGV), and a Rust panic's exit status.
        for (name, body, want) in [("segv", "kill -SEGV $$", "signal 11"), ("panic", "exit 101", "status 101")] {
            let probe = s.probe(name, body);
            let v = verdict(Some(&cache), command(&probe, &lib64), PROBE_TIMEOUT);
            assert!(matches!(&v, Verdict::Unclear(why) if why.contains(want)), "{name}: {v:?}");
            assert!(!cache.exists(), "{name} was cached: {:?}", std::fs::read_to_string(&cache));
        }
    }

    #[test]
    fn a_probe_that_is_killed_is_not_cached() {
        let s = Scratch::new("killed");
        let lib64 = s.lib64();
        let cache = s.0.join("cache/kosmickrisp");
        // SIGKILL (jetsam on a dense farm) and SIGTERM (its process group stopped).
        for (name, body, want) in [("kill", "kill -KILL $$", "signal 9"), ("term", "kill -TERM $$", "signal 15")] {
            let probe = s.probe(name, body);
            let v = verdict(Some(&cache), command(&probe, &lib64), PROBE_TIMEOUT);
            assert!(matches!(&v, Verdict::Unclear(why) if why.contains(want)), "{name}: {v:?}");
            assert!(!cache.exists(), "{name} was cached: {:?}", std::fs::read_to_string(&cache));
        }
    }

    #[test]
    fn a_clean_verdict_is_cached_and_read_back() {
        let s = Scratch::new("clean");
        let lib64 = s.lib64();
        let crash = s.probe("crash", "kill -SEGV $$");
        let pass = s.probe("pass", "exit 0");
        let fail = s.probe("fail", &format!("exit {PROBE_FAILED}"));
        let (ok_cache, fail_cache) = (s.0.join("cache/ok"), s.0.join("cache/fail"));
        assert_eq!(verdict(Some(&ok_cache), command(&pass, &lib64), PROBE_TIMEOUT), Verdict::Pass);
        assert_eq!(std::fs::read_to_string(&ok_cache).expect("cached"), "ok");
        assert_eq!(verdict(Some(&fail_cache), command(&fail, &lib64), PROBE_TIMEOUT), Verdict::Fail);
        assert_eq!(std::fs::read_to_string(&fail_cache).expect("cached"), "fail");
        // Read back without probing again (this probe would crash).
        assert_eq!(verdict(Some(&ok_cache), command(&crash, &lib64), PROBE_TIMEOUT), Verdict::Pass);
        assert_eq!(verdict(Some(&fail_cache), command(&crash, &lib64), PROBE_TIMEOUT), Verdict::Fail);
    }

    #[test]
    fn a_half_written_cache_file_is_no_verdict() {
        // A boot that reads the file while another writes it sees it empty: that is no verdict,
        // not a failure.
        let s = Scratch::new("partial");
        let lib64 = s.lib64();
        let cache = s.0.join("cache/kosmickrisp");
        std::fs::create_dir_all(s.0.join("cache")).expect("dir");
        std::fs::write(&cache, "").expect("empty");
        let pass = s.probe("pass", "exit 0");
        assert_eq!(verdict(Some(&cache), command(&pass, &lib64), PROBE_TIMEOUT), Verdict::Pass);
        assert_eq!(std::fs::read_to_string(&cache).expect("cached"), "ok");
        // No temporary file is left beside it.
        let left: Vec<_> = std::fs::read_dir(s.0.join("cache")).expect("ls").flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, [OsString::from("kosmickrisp")]);
    }

    #[test]
    fn a_probe_child_sees_only_its_own_driver_settings() {
        let s = Scratch::new("env");
        let lib64 = s.lib64();
        let out = s.0.join("env.txt");
        let dump = s.probe("dump", &format!("echo \"args $1\" > {0}\n/usr/bin/env >> {0}", out.display()));
        // What a user, a bench arm or an earlier configure_environment in this process left.
        let inherited = env(&[
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/Users/someone"),
            ("ANDROID_EMU_VK_ICD", "lavapipe"),
            ("VK_DRIVER_FILES", "/elsewhere/lvp_icd.json"),
            ("VK_ICD_FILENAMES", "/elsewhere/lvp_icd.json"),
            ("VK_ADD_DRIVER_FILES", "/elsewhere/extra_icd.json"),
            ("VK_LOADER_DRIVERS_SELECT", "*lvp*"),
            ("VK_INSTANCE_LAYERS", "VK_LAYER_KHRONOS_validation"),
            ("VK_LAYER_PATH", "/elsewhere/layers"),
            ("ANDROID_EMU_VK_LOADER_PATH", "/elsewhere/libvulkan.dylib"),
            ("ANDROID_EMULATOR_LAUNCHER_DIR", "/elsewhere"),
            ("MVK_CONFIG_USE_METAL_ARGUMENT_BUFFERS", "1"),
            ("MESA_VK_DEVICE_SELECT", "1234:5678"),
        ]);
        for icd in [Icd::MoltenVk, Icd::KosmicKrisp] {
            let cmd = probe_command(&dump, probe_flag(icd), &lib64, icd, inherited.clone());
            assert_eq!(run(cmd, PROBE_TIMEOUT), Verdict::Pass);
            let text = std::fs::read_to_string(&out).expect("env dump");
            let seen: std::collections::HashMap<&str, &str> = text.lines().skip(1).filter_map(|l| l.split_once('=')).collect();
            assert_eq!(text.lines().next(), Some(format!("args {}", probe_flag(icd)).as_str()));
            let manifest = lib64.join("vulkan").join(icd.manifest());
            let manifest = manifest.to_str().expect("utf-8");
            let want = [("ANDROID_EMU_VK_ICD", icd.name()), ("VK_DRIVER_FILES", manifest), ("VK_ICD_FILENAMES", manifest)];
            for (k, v) in want {
                assert_eq!(seen.get(k).copied(), Some(v), "{icd:?}: {k}");
            }
            for k in [
                "VK_ADD_DRIVER_FILES",
                "VK_LOADER_DRIVERS_SELECT",
                "VK_INSTANCE_LAYERS",
                "VK_LAYER_PATH",
                "ANDROID_EMU_VK_LOADER_PATH",
                "ANDROID_EMULATOR_LAUNCHER_DIR",
                "MVK_CONFIG_USE_METAL_ARGUMENT_BUFFERS",
                "MESA_VK_DEVICE_SELECT",
            ] {
                assert_eq!(seen.get(k), None, "{icd:?}: {k} reached the probe");
            }
            // Everything else is inherited.
            assert_eq!(seen.get("PATH").copied(), Some("/usr/bin:/bin"));
            assert_eq!(seen.get("HOME").copied(), Some("/Users/someone"));
        }
    }

    #[test]
    fn the_cache_key_follows_the_backend_and_the_driver() {
        let s = Scratch::new("key");
        let lib64 = s.lib64();
        let dir = s.0.join("cache");
        let key = |icd| cache_file_in(&dir, &lib64, icd).expect("key");
        let (mvk, kk) = (key(Icd::MoltenVk), key(Icd::KosmicKrisp));
        assert_eq!(key(Icd::KosmicKrisp), kk);
        // Each driver's verdict has a file of its own.
        assert_ne!(mvk, kk);
        assert!(kk.file_name().expect("name").to_string_lossy().starts_with("kosmickrisp-"));
        assert!(mvk.file_name().expect("name").to_string_lossy().starts_with("moltenvk-"));
        // KosmicKrisp's library replaced in place (same backend): a new key for it alone.
        let driver = lib64.join("vulkan/libvulkan_kosmickrisp.dylib");
        std::fs::remove_file(&driver).expect("rm");
        std::fs::write(&driver, b"kosmickrisp, rebuilt").expect("new driver");
        assert_ne!(key(Icd::KosmicKrisp), kk, "a replaced KosmicKrisp keeps its old verdict");
        assert_eq!(key(Icd::MoltenVk), mvk);
        // A new backend: new keys for both.
        let backend = lib64.join("libgfxstream_backend.dylib");
        std::fs::remove_file(&backend).expect("rm");
        std::fs::write(&backend, b"backend, rebuilt").expect("new backend");
        assert_ne!(key(Icd::MoltenVk), mvk);
    }

    #[test]
    fn stamp_follows_the_build() {
        let s = Scratch::new("stamp");
        let (a, b) = (s.0.join("a"), s.0.join("b"));
        std::fs::write(&a, b"build one").expect("a");
        std::fs::write(&b, b"build two!").expect("b");
        let st = |p: &Path| stamp(&std::fs::metadata(p).expect("meta"));
        assert_eq!(st(&a), st(&a));
        // Written in the same second: size and inode still tell them apart.
        assert_ne!(st(&a), st(&b));
    }
}
