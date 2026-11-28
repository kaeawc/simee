//! XR bodies (visionOS, Android XR headsets and AI glasses): none yet.
//!
//! The form factor's module, as [`super::table`] asks of each: [`ENABLED`], [`rows`] and
//! [`generic`]. A headset (visor shell, a `Role::Trim` light seal, `Role::Strap` head straps, the
//! frame on its eye-side face) and glasses (a front bar, `Role::Lens` lenses, `Role::Strap`
//! temples; the SDK's `ai_glasses_device` screen, `form_factor::ANDROID_GLASSES_SCREEN`) go here,
//! nowhere else: set [`ENABLED`] and the table, [`super::has_body`], the mesh checks and the
//! desktop's face-on test take them up.

use super::Body;
use super::table::Spec;

/// Headsets and glasses have no body yet ([`super::has_body`]).
pub(super) const ENABLED: bool = false;

/// Whether the generic body's screen is drawn at the dpi simee reports for the Android reference
/// profile (a headset's frame spans the eyepiece area, not the reported dpi; its lane may change this). When false, the shared test checks only the aspect.
#[cfg(test)]
pub(super) const SIZES_BY_DPI: bool = false;

/// The headset and glasses rows of the table (read only while [`ENABLED`]).
pub(super) fn rows() -> Vec<Body> {
    Vec::new()
}

/// A headset or glasses with no row (asked only while [`ENABLED`]).
pub(super) fn generic(_: &Spec) -> Option<Body> {
    None
}
