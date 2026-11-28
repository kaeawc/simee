// Lets the binary find the gfxstream renderer's libraries at runtime: next to the binary (an SDK
// overlay installs emu as emulator/emulator; a target/ build gets a lib64 symlink at startup),
// then in the build machine's SDK, either its emulator package or the renderer that
// `emu -provision-renderer` keeps under simee/renderer/current. The renderer's @rpath
// dependencies are preloaded by emu-gfxstream and don't need these; libraries gfxstream
// dlopens by bare name (libshadertranslator.dylib) do.
fn main() {
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_SDK_ROOT");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // cpal's CoreAudio backend imports AudioHardwareCreateProcessTap and ...DestroyProcessTap
    // (macOS 14.2). Linked strongly, dyld refuses to load emu on macOS 13 and 14.0 to 14.1 before
    // `main` can name the floor. Weak, they resolve to null there; emu refuses below macOS 15
    // first, so nothing calls them. scripts/check-imports.sh checks the result.
    println!("cargo:rustc-link-arg-bins=-Wl,-weak_framework,CoreAudio");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/lib64");
    // Host GLES is ANGLE on Metal; gfxstream dlopens libEGL/libGLESv2 by bare name.
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/lib64/gles_angle");
    let sdk = std::env::var("ANDROID_SDK_ROOT")
        .or_else(|_| std::env::var("ANDROID_HOME"))
        .unwrap_or_else(|_| format!("{}/Library/Android/sdk", std::env::var("HOME").unwrap_or_default()));
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/emulator/lib64");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/simee/renderer/current/lib64");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/emulator/lib64/gles_angle");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/simee/renderer/current/lib64/gles_angle");
    info_plist();
}

/// emu's bundle identifier. Metal keeps its shader cache per bundle
/// (`$DARWIN_USER_CACHE_DIR/<id>/com.apple.metal`); a process without one shares a single cache
/// with every other unbundled process of the user (CLI tools, test binaries), which other work
/// fills and rotates. The renderer's init compiles ANGLE's Metal shader library (about 4 s), so
/// a miss there stalls the boot. Changing the id gives emu an empty cache once.
const BUNDLE_ID: &str = "dev.simee.emu";

/// Embeds an `Info.plist` naming [`BUNDLE_ID`] in the binary's `__TEXT,__info_plist` section,
/// which is where CoreFoundation reads an unbundled executable's main-bundle info from. The
/// microphone usage string is there because a binary with an Info.plist needs one before macOS
/// lets it record (virtio-snd capture) when it is its own responsible process.
fn info_plist() {
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
<key>CFBundleName</key><string>emu</string>
<key>CFBundleShortVersionString</key><string>{}</string>
<key>NSMicrophoneUsageDescription</key><string>The Android emulator passes host audio input to the guest's microphone.</string>
</dict>
</plist>
"#,
        std::env::var("CARGO_PKG_VERSION").unwrap_or_default()
    );
    let Some(out) = std::env::var_os("OUT_DIR") else { return };
    let path = std::path::Path::new(&out).join("Info.plist");
    if std::fs::write(&path, plist).is_ok() {
        println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}", path.display());
    }
}
