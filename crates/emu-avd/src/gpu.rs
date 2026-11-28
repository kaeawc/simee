//! `-gpu` modes and the AVD's `hw.gpu.mode`: which Vulkan ICD gfxstream runs on.
//!
//! The mode names are the stock emulator's. SDK images ship no guest renderer (no
//! API 34-37 image carries one), so `guest` and `software` can't render in the guest; like
//! stock, they fall back to lavapipe and say so.

/// Every mode `-gpu` accepts, stock's current names first, then its legacy ones.
pub const MODES: &[&str] =
    &["auto", "host", "software", "lavapipe", "swiftshader", "swangle", "off", "none", "swiftshader_indirect", "angle_indirect", "guest"];

/// What the mode runs the renderer on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The host GPU through MoltenVK, falling back to software if it fails on this machine.
    Host,
    SwiftShader,
    Lavapipe,
    /// No renderer at all (`-gpu off`).
    Off,
}

/// The decision for one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuChoice {
    pub backend: Backend,
    /// The mode asked for in-guest rendering, which images can't do: print stock's fallback notice.
    pub guest_fallback: bool,
    /// A notice to print once before the boot (a mode this host can't run was swapped).
    pub notice: Option<&'static str>,
}

/// Stock's notice when an AVD wants in-guest rendering and the image can't (adapted).
pub const GUEST_FALLBACK_NOTICE: &str = "emu: Your AVD has been configured with an in-guest renderer, but the system image \
     does not support guest rendering. Falling back to 'lavapipe' mode.";

/// Why `-gpu swiftshader` is refused on macOS hosts. The SDK's SwiftShader ICD aborts the
/// renderer at init there ("Failed to find memory type for ColorBuffers", 6 of 6 boots on API 30
/// and 36, #397): it exposes no memory type the ColorBuffer allocator accepts.
pub const SWIFTSHADER_REFUSAL: &str = "-gpu swiftshader isn't supported on macOS hosts (the renderer aborts at init with \
     \"Failed to find memory type for ColorBuffers\"): use -gpu lavapipe";

/// Notice when `hw.gpu.mode` asks for SwiftShader on a macOS host and lavapipe runs instead.
pub const SWIFTSHADER_FALLBACK_NOTICE: &str = "emu: hw.gpu.mode=swiftshader isn't supported on macOS hosts (the renderer aborts at \
     init). Falling back to 'lavapipe' mode.";

/// Parse a `-gpu` argument; an unknown name is an error that lists the valid ones.
pub fn parse_flag(mode: &str) -> Result<(), String> {
    parse_flag_on(mode, cfg!(target_os = "macos"))
}

fn parse_flag_on(mode: &str, macos: bool) -> Result<(), String> {
    // Only the literal name is refused: `swiftshader_indirect` is the legacy name tools (AutoMobile's
    // harness launch line) hard-code, so it falls back in `resolve_on` instead of failing them.
    if macos && mode == "swiftshader" {
        return Err(SWIFTSHADER_REFUSAL.to_string());
    }
    if MODES.contains(&mode) { Ok(()) } else { Err(format!("unknown -gpu mode '{mode}'; valid modes: {}", MODES.join(", "))) }
}

fn choose(mode: &str) -> Option<GpuChoice> {
    let (backend, guest_fallback) = match mode {
        "auto" | "host" => (Backend::Host, false),
        "off" | "none" => (Backend::Off, false),
        "swiftshader" | "swiftshader_indirect" => (Backend::SwiftShader, false),
        "lavapipe" | "swangle" | "angle_indirect" => (Backend::Lavapipe, false),
        "software" | "guest" => (Backend::Lavapipe, true),
        _ => return None,
    };
    Some(GpuChoice { backend, guest_fallback, notice: None })
}

/// Resolve the mode: the `-gpu` flag beats the AVD's `hw.gpu.mode`. An `off` in `config.ini`
/// (and any name this doesn't know) keeps today's default, the host renderer; only the flag
/// turns rendering off.
pub fn resolve(flag: Option<&str>, config_mode: Option<&str>) -> GpuChoice {
    resolve_on(flag, config_mode, cfg!(target_os = "macos"))
}

fn resolve_on(flag: Option<&str>, config_mode: Option<&str>, macos: bool) -> GpuChoice {
    let host = GpuChoice { backend: Backend::Host, guest_fallback: false, notice: None };
    let choice = match flag {
        Some(f) => choose(f).unwrap_or(host),
        None => match config_mode.and_then(choose) {
            Some(c) if c.backend != Backend::Off => c,
            _ => host,
        },
    };
    // `parse_flag` refuses the explicit flag up front; a config default (or a flag that skipped
    // it) is swapped for lavapipe instead of aborting at renderer init.
    if macos && choice.backend == Backend::SwiftShader {
        return GpuChoice { backend: Backend::Lavapipe, guest_fallback: false, notice: Some(SWIFTSHADER_FALLBACK_NOTICE) };
    }
    choice
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stock_mode_parses_and_unknown_lists_them() {
        for m in MODES {
            assert!(parse_flag_on(m, false).is_ok(), "{m}");
            assert!(choose(m).is_some(), "{m} has no mapping");
        }
        let e = parse_flag("vulkan").expect_err("unknown mode");
        assert!(e.contains("'vulkan'") && e.contains("lavapipe") && e.contains("swangle"), "{e}");
    }

    #[test]
    fn modes_map_to_backends() {
        let b = |m| resolve_on(Some(m), None, false).backend;
        assert_eq!(b("host"), Backend::Host);
        assert_eq!(b("swiftshader_indirect"), Backend::SwiftShader);
        assert_eq!(b("lavapipe"), Backend::Lavapipe);
        assert_eq!(b("swangle"), Backend::Lavapipe);
        assert_eq!(b("off"), Backend::Off);
        assert!(!resolve(Some("lavapipe"), None).guest_fallback);
    }

    #[test]
    fn guest_and_software_use_lavapipe_with_the_notice() {
        for m in ["guest", "software"] {
            assert_eq!(resolve(Some(m), None), GpuChoice { backend: Backend::Lavapipe, guest_fallback: true, notice: None });
            assert_eq!(resolve(None, Some(m)), GpuChoice { backend: Backend::Lavapipe, guest_fallback: true, notice: None });
        }
    }

    #[test]
    fn flag_beats_config() {
        assert_eq!(resolve(Some("host"), Some("guest")), GpuChoice { backend: Backend::Host, guest_fallback: false, notice: None });
        assert_eq!(resolve_on(Some("swiftshader"), Some("software"), false).backend, Backend::SwiftShader);
        assert_eq!(resolve(Some("off"), Some("auto")).backend, Backend::Off);
    }

    #[test]
    fn explicit_swiftshader_is_refused_on_macos() {
        let e = parse_flag_on("swiftshader", true).expect_err("refused on macOS");
        assert!(e.contains("isn't supported on macOS") && e.contains("-gpu lavapipe"), "{e}");
        assert!(parse_flag_on("swiftshader", false).is_ok(), "stays valid elsewhere");
        // The legacy name tools hard-code parses and falls back instead.
        assert!(parse_flag_on("swiftshader_indirect", true).is_ok());
    }

    #[test]
    fn config_swiftshader_falls_back_to_lavapipe_on_macos_with_a_notice() {
        for m in ["swiftshader", "swiftshader_indirect"] {
            let c = resolve_on(None, Some(m), true);
            assert_eq!(c.backend, Backend::Lavapipe, "{m}");
            assert!(c.notice.is_some_and(|n| n.contains("lavapipe")), "{m}");
            let c = resolve_on(Some(m), None, true);
            assert_eq!(c.backend, Backend::Lavapipe, "a flag that slipped past parse_flag never reaches the ICD");
            assert!(c.notice.is_some());
            // Elsewhere SwiftShader is unchanged.
            assert_eq!(resolve_on(None, Some(m), false), GpuChoice { backend: Backend::SwiftShader, guest_fallback: false, notice: None });
        }
    }

    #[test]
    fn other_modes_are_the_same_on_macos() {
        for m in MODES.iter().filter(|m| !m.starts_with("swiftshader")) {
            assert_eq!(resolve_on(Some(m), None, true), resolve_on(Some(m), None, false), "{m}");
            assert_eq!(resolve_on(None, Some(m), true), resolve_on(None, Some(m), false), "{m}");
            assert!(parse_flag_on(m, true).is_ok(), "{m}");
            assert!(resolve_on(Some(m), None, true).notice.is_none(), "{m}");
        }
    }

    #[test]
    fn default_is_unchanged() {
        let host = GpuChoice { backend: Backend::Host, guest_fallback: false, notice: None };
        assert_eq!(resolve(None, None), host);
        assert_eq!(resolve(None, Some("auto")), host);
        assert_eq!(resolve(None, Some("off")), host);
        assert_eq!(resolve(None, Some("mesa")), host);
    }
}
