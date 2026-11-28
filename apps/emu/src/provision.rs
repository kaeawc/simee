//! `-provision`, `-provision-renderer` and `-list-images`: install what simee needs
//! (platform-tools, a system image and the gfxstream renderer) straight from Google's
//! repository, with no Java, sdkmanager or avdmanager.
use std::path::PathBuf;

use anyhow::{Result, bail};
use emu_avd::Env;
use emu_sdk::{Host, Sdk, renderer};

use crate::args::Opts;

/// Whether this host boots guests (and so needs the renderer): Apple silicon, and x86_64 Linux in
/// software mode.
const BOOTS: bool = cfg!(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")));
const DEFAULT_TAG: &str = "google_apis";
/// The images this host boots: software mode on x86_64 Linux runs x86_64 images only.
const DEFAULT_ABI: &str = if cfg!(all(target_os = "linux", target_arch = "x86_64")) { "x86_64" } else { "arm64-v8a" };

/// The SDK to provision: the resolved SDK root, else the platform default; created if missing.
fn sdk(env: &Env) -> Result<Sdk> {
    let root = env.sdk_root.clone().unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        home.join(emu_avd::DEFAULT_SDK)
    });
    Ok(Sdk::new(root)?)
}

pub fn list_images(env: &Env, opts: &Opts) -> Result<()> {
    let sdk = sdk(env)?;
    let abi = opts.abi.as_deref().unwrap_or(DEFAULT_ABI);
    println!("{:<16} {:<28} {:>5} {:>9}  installed", "api", "tag", "rev", "size");
    let mut n = 0;
    for m in sdk.system_image_manifests(opts.tag.as_deref())? {
        for d in m.system_images(Host::current(), opts.tag.as_deref(), abi) {
            let Some(p) = emu_sdk::SysImagePath::parse(&d.path) else { continue };
            let api = p.api();
            let have = if sdk.is_installed(&d.install_dir()) { "yes" } else { "" };
            println!("{api:<16} {:<28} {:>5} {:>9}  {have}", p.tag, d.revision.to_string(), emu_sdk::format_size(d.size));
            n += 1;
        }
    }
    if n == 0 {
        bail!(
            "no stable {abi} system images for {}{}",
            Host::current(),
            opts.tag.as_ref().map(|t| format!(" with tag {t}")).unwrap_or_default()
        );
    }
    Ok(())
}

pub fn provision(env: &Env, api: &str, opts: &Opts) -> Result<()> {
    let sdk = sdk(env)?;
    let tag = opts.tag.as_deref().unwrap_or(DEFAULT_TAG);
    let abi = opts.abi.as_deref().unwrap_or(DEFAULT_ABI);
    // Refuse before a multi-gigabyte download, not after (also under -dry-run, which says so).
    if let Some(profile) = &opts.profile {
        emu_avd::check_profile(profile, tag)?;
    }
    if let Some(name) = &opts.create_avd
        && (env.avd_home.join(format!("{name}.avd")).exists() || env.avd_home.join(format!("{name}.ini")).exists())
    {
        bail!("AVD '{name}' already exists");
    }
    println!("SDK: {}", sdk.root().display());

    install(&sdk, opts, "platform-tools", || sdk.manifest(&emu_sdk::repository_url())?.resolve("platform-tools", Host::current()))?;
    let mut image = emu_sdk::system_image_path(api, tag, abi);
    if sdk.is_installed(&emu_sdk::install_dir(&image)) {
        println!("have {image}");
    } else {
        let d = match sdk.find_system_image(api, tag, abi, Host::current()) {
            Err(e @ (emu_sdk::Error::UnknownPackage(..) | emu_sdk::Error::Command(..))) => {
                bail!("{e} (`emu -list-images -tag {tag}` shows what is available)")
            }
            r => r?,
        };
        if d.path != image {
            println!("{image} isn't published; {} is", d.path);
        }
        image.clone_from(&d.path);
        install(&sdk, opts, &image, || Ok(d))?;
    }
    if !opts.no_renderer {
        if BOOTS {
            renderer(&sdk, opts, opts.provision_renderer)?;
        } else {
            println!(
                "skipping the renderer: emu boots guests only on Apple silicon and x86_64 Linux (-provision-renderer installs it anyway)"
            );
        }
    }

    if let Some(name) = &opts.create_avd {
        let sysdir = emu_sdk::install_dir(&image).to_string_lossy().into_owned();
        if opts.dry_run {
            println!("would create AVD {name} from {sysdir}");
        } else {
            let env = Env { sdk_root: Some(sdk.root().to_path_buf()), ..env.clone() };
            crate::create_avd(&env, name, &sysdir, opts)?;
        }
    }
    Ok(())
}

/// Installs package `path` unless the SDK has it; `resolve` finds its download. With `-dry-run`,
/// prints what would be downloaded instead.
fn install(sdk: &Sdk, opts: &Opts, path: &str, resolve: impl FnOnce() -> emu_sdk::Result<emu_sdk::Download>) -> Result<()> {
    let dir = emu_sdk::install_dir(path);
    if sdk.is_installed(&dir) {
        println!("have {path}");
        return Ok(());
    }
    let d = resolve()?;
    if opts.dry_run {
        println!("{path} r{}", d.revision);
        println!("  url  {}", d.url);
        println!("  size {} ({})", d.size, emu_sdk::format_size(d.size));
        println!("  sha1 {}", d.sha1);
        println!("  into {}", sdk.root().join(&dir).display());
        return Ok(());
    }
    println!("installing {path} r{}", d.revision);
    let dest = sdk.install(&d)?;
    println!("installed {path} r{} at {}", d.revision, dest.display());
    Ok(())
}

/// `-provision-renderer`: install (or update to) the newest renderer.
pub fn provision_renderer(env: &Env, opts: &Opts) -> Result<()> {
    let sdk = sdk(env)?;
    println!("SDK: {}", sdk.root().display());
    renderer(&sdk, opts, true)
}

/// Installs the gfxstream renderer out of the `emulator` package (see [`install_renderer`]),
/// then, unless it's a dry run, initializes it once so the first boot finds Metal's shader
/// cache warm ([`warm_renderer`]).
fn renderer(sdk: &Sdk, opts: &Opts, update: bool) -> Result<()> {
    install_renderer(sdk, opts, update)?;
    if !opts.dry_run {
        warm_renderer(sdk.root());
    }
    Ok(())
}

/// Initializes the renderer boots will use once, in a child process: its init compiles ANGLE's
/// Metal shader library (about 4 s), which then sits in emu's own shader cache, so the first
/// boot doesn't wait for it.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn warm_renderer(root: &std::path::Path) {
    let Some(lib64) = renderer::find_lib64(root) else { return };
    let (ok, warmed, took) = crate::vkprobe::warm(&lib64);
    if !ok {
        println!("renderer probed in {:.1} s; MoltenVK didn't come up, so boots use lavapipe (software Vulkan)", took.as_secs_f64());
    } else if warmed {
        println!("renderer initialized in {:.1} s, its Metal shaders now cached", took.as_secs_f64());
    } else {
        eprintln!("emu: warming the renderer failed after {:.1} s; the first boot compiles its shaders instead", took.as_secs_f64());
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn warm_renderer(_: &std::path::Path) {}

/// Installs the gfxstream renderer out of the `emulator` package. With `update` the newest
/// stable revision is installed unless it already is; without it (plain `-provision`) any
/// renderer the SDK already has will do, including the SDK's own emulator package.
fn install_renderer(sdk: &Sdk, opts: &Opts, update: bool) -> Result<()> {
    let root = sdk.root();
    let sdk_package = renderer::sdk_emulator_lib64(root);
    let installed = renderer::installed(root);
    let existing = sdk_package.clone().or_else(|| installed.first().map(renderer::Installed::lib64));
    if !update
        && !opts.dry_run
        && let Some(lib64) = &existing
    {
        println!("have renderer ({})", lib64.display());
        return Ok(());
    }
    let d = sdk.manifest(&emu_sdk::repository_url())?.resolve(renderer::PACKAGE, renderer::host())?;
    let have = installed.iter().find(|i| i.marker.revision == d.revision);
    if opts.dry_run {
        let state = match (have, &existing) {
            (Some(_), _) => " (installed)".to_string(),
            (None, Some(lib64)) if !update => format!(" (not needed: have {})", lib64.display()),
            _ => String::new(),
        };
        println!("renderer: {} r{}{state}", renderer::PACKAGE, d.revision);
        println!("  url  {}", d.url);
        println!("  size {} ({})", d.size, emu_sdk::format_size(d.size));
        println!("  sha1 {}", d.sha1);
        println!("  only {} (not {})", renderer::INCLUDE.join(" "), renderer::EXCLUDE.join(" "));
        println!("  into {}", renderer::dir(root, d.revision).join("lib64").display());
        return Ok(());
    }
    if let Some(i) = have {
        println!("have renderer r{} ({})", d.revision, i.lib64().display());
        return Ok(());
    }
    if let Some(lib64) = &sdk_package {
        println!("note: the SDK's emulator package ({}) stays the renderer emu uses while it is installed", lib64.display());
    }
    println!("installing renderer r{} (lib64 of the {} package)", d.revision, renderer::PACKAGE);
    let inst = sdk.install_renderer(&d)?;
    println!(
        "installed renderer r{} at {} ({} on disk, from a {} archive)",
        d.revision,
        inst.lib64().display(),
        emu_sdk::format_size(renderer::disk_usage(&inst.dir)),
        emu_sdk::format_size(d.size)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(line: &str) -> Opts {
        crate::args::parse(line.split(' ').map(String::from)).expect("parse")
    }

    #[test]
    fn a_profile_the_tag_refuses_is_refused_before_anything_is_fetched() {
        let root = std::env::temp_dir().join(format!("emu-provision-profile-{}", std::process::id()));
        let env = Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        for dry in ["", " -dry-run"] {
            let line = format!("-provision 34 -tag android-tv -create-avd tv -profile software-lite{dry}");
            let err = provision(&env, "34", &opts(&line)).expect_err("a TV has no screen to shrink").to_string();
            assert!(err.contains("software-lite") && err.contains("android-tv"), "{err}");
        }
        // No manifest was fetched and nothing was installed: the SDK directory has no files.
        let files = |d: &std::path::Path| std::fs::read_dir(d).map(|r| r.count()).unwrap_or(0);
        assert_eq!(files(&root.join("sdk")), 0, "nothing under the SDK");
        let _ = std::fs::remove_dir_all(root);
    }
}
