//! Warns when the `openh264` feature builds OpenH264 on x86 without `nasm`: `openh264-sys2` then
//! skips the assembly without saying so, and the encoder is about 2.4 times slower
//! (`docs/streaming.md`, "Encode cost on Linux"). Does nothing without the feature or off x86
//! Linux.

use std::process::{Command, Stdio};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");
    if std::env::var_os("CARGO_FEATURE_OPENH264").is_none() {
        return;
    }
    let target = |key: &str| std::env::var(key).unwrap_or_default();
    if target("CARGO_CFG_TARGET_OS") != "linux" || !matches!(target("CARGO_CFG_TARGET_ARCH").as_str(), "x86" | "x86_64") {
        return;
    }
    if std::env::var_os("OPENH264_NO_ASM").is_some() {
        println!("cargo:warning=OPENH264_NO_ASM is set: OpenH264 is built without its x86 assembly, about 2.4x slower");
        return;
    }
    let found = Command::new("nasm").arg("-v").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !found {
        println!(
            "cargo:warning=nasm not found: OpenH264 is built without its x86 assembly, about 2.4x slower \
             (install nasm, then `cargo clean -p openh264-sys2`; docs/streaming.md, \"Linux: OpenH264\")"
        );
    }
}
