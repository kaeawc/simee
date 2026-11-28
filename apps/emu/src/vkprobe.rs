//! One-time check that gfxstream can bring up Vulkan on MoltenVK (and on KosmicKrisp, for the
//! guests that run on it). The renderer initializes once per process and silently disables
//! Vulkan on failure, so the check runs in a child process (`emu -probe-vulkan <lib64>`,
//! `emu -probe-kosmickrisp <lib64>`) and its verdict is cached per renderer build.
use std::path::{Path, PathBuf};

fn cache_file(lib64: &Path) -> Option<PathBuf> {
    cache_file_for(lib64, emu_gfxstream::Icd::MoltenVk)
}

fn cache_file_for(lib64: &Path, icd: emu_gfxstream::Icd) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let meta = std::fs::metadata(lib64.join("libgfxstream_backend.dylib")).ok()?;
    Some(PathBuf::from(home).join("Library/Caches/simee").join(stamp_name(icd, &meta)))
}

/// One renderer build: its backend's size, mtime (ns) and inode. Builds installed within the same
/// second (or copied with preserved mtimes) still differ by size or inode.
fn stamp_name(icd: emu_gfxstream::Icd, meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}-{}.{:09}-{}-{}", icd.name(), meta.mtime(), meta.mtime_nsec(), meta.len(), meta.ino())
}

/// Cached verdict, probing (≈1 s, once) when unknown.
pub fn moltenvk_works(lib64: &Path) -> bool {
    let cache = cache_file(lib64);
    if let Some(c) = &cache
        && let Ok(v) = std::fs::read_to_string(c)
    {
        return v.trim() == "ok";
    }
    probe_and_record(lib64, cache)
}

/// Whether gfxstream brings up Vulkan on KosmicKrisp: cached like [`moltenvk_works`], probing
/// (`emu -probe-kosmickrisp`) once per renderer build.
pub fn kosmickrisp_works(lib64: &Path) -> bool {
    let cache = cache_file_for(lib64, emu_gfxstream::Icd::KosmicKrisp);
    if let Some(c) = &cache
        && let Ok(v) = std::fs::read_to_string(c)
    {
        return v.trim() == "ok";
    }
    let ok = child("-probe-kosmickrisp", lib64);
    record(cache, ok);
    ok
}

fn record(cache: Option<PathBuf>, ok: bool) {
    if let Some(c) = cache {
        let _ = c.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(c, if ok { "ok" } else { "fail" });
    }
}

/// Runs `emu <flag> <lib64>` and reports whether it succeeded.
fn child(flag: &str, lib64: &Path) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| std::process::Command::new(exe).arg(flag).arg(lib64).stderr(std::process::Stdio::null()).output().ok())
        .is_some_and(|o| o.status.success())
}

/// Runs the probe child and records its verdict.
fn probe_and_record(lib64: &Path, cache: Option<PathBuf>) -> bool {
    let ok = child("-probe-vulkan", lib64);
    record(cache, ok);
    ok
}

/// Initializes the renderer in children the way boots do (`-provision-renderer` runs it), so the
/// Metal shaders a boot's renderer init compiles (about 4 s cold, ANGLE's shader library most of
/// it) are in emu's shader cache before the first boot: the probe's init (a GL backend, as API 31
/// to 35 guests get), then, when MoltenVK works, an API 36 guest's (no GL, Vulkan composition),
/// which compiles about another 0.9 s the probe's doesn't. Records the MoltenVK verdict too.
/// Returns whether Vulkan came up, whether the API 36 init ran to the end (false when Vulkan
/// didn't come up or that child failed), and how long the children took.
pub fn warm(lib64: &Path) -> (bool, bool, std::time::Duration) {
    let t = std::time::Instant::now();
    let ok = probe_and_record(lib64, cache_file(lib64));
    let warmed = ok && child("-warm-renderer", lib64);
    (ok, warmed, t.elapsed())
}

/// Child side of [`warm`]: the renderer's init exactly as an API 36 boot on MoltenVK makes it,
/// host ANGLE included (`instance::host_angle`).
pub fn warm_child(lib64: &Path) -> bool {
    let Some(emulator_dir) = lib64.parent() else { return false };
    emu_gfxstream::link_beside_exe(lib64);
    if emu_vmm::runs_without_gl(true, lib64) {
        emu_gfxstream::prefer_angle_platform("swiftshader");
    }
    emu_gfxstream::configure_environment(emulator_dir, emu_gfxstream::Icd::MoltenVk);
    emu_vmm::warm_renderer(lib64.to_path_buf(), true, true).is_ok()
}

/// Child side: init the renderer on MoltenVK and report whether Vulkan came up.
pub fn probe(lib64: &Path) -> bool {
    probe_on(lib64, emu_gfxstream::Icd::MoltenVk)
}

/// Child side of [`kosmickrisp_works`]: [`probe`] on KosmicKrisp.
pub fn probe_kosmickrisp(lib64: &Path) -> bool {
    probe_on(lib64, emu_gfxstream::Icd::KosmicKrisp)
}

fn probe_on(lib64: &Path, icd: emu_gfxstream::Icd) -> bool {
    let Some(emulator_dir) = lib64.parent() else { return false };
    emu_gfxstream::configure_environment(emulator_dir, icd);
    let Ok(r) = emu_gfxstream::Renderer::load(lib64) else { return false };
    let flags = emu_gfxstream::flags::USE_GLES | emu_gfxstream::flags::USE_VK;
    r.init(320, 240, flags, false, std::sync::Arc::new(|_| {})).is_ok() && emu_gfxstream::vulkan_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_follows_the_build() {
        let dir = std::env::temp_dir().join(format!("emu-vkprobe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, b"build one").expect("a");
        std::fs::write(&b, b"build two!").expect("b");
        let name = |p: &Path| stamp_name(emu_gfxstream::Icd::MoltenVk, &std::fs::metadata(p).expect("meta"));
        assert_eq!(name(&a), name(&a));
        // Written in the same second: size and inode still tell them apart.
        assert_ne!(name(&a), name(&b));
        assert!(name(&a).starts_with("moltenvk-"));
        // Each driver's verdict has a file of its own.
        let kk = stamp_name(emu_gfxstream::Icd::KosmicKrisp, &std::fs::metadata(&a).expect("meta"));
        assert_eq!(kk, name(&a).replacen("moltenvk-", "kosmickrisp-", 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
