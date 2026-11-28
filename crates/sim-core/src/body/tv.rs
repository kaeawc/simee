//! TV bodies (Apple TV, Android TV / Google TV): none yet.
//!
//! The form factor's module, as [`super::table`] asks of each: [`ENABLED`], [`rows`] and
//! [`generic`]. A television (panel, thin bezel, a pedestal or feet as `Role::Stand`
//! [`super::Block`]s) goes here, nowhere else: set [`ENABLED`] and the table, [`super::has_body`],
//! the mesh checks and the desktop's face-on test take it up.

use super::Body;
use super::table::Spec;

/// TVs have no body yet ([`super::has_body`]).
pub(super) const ENABLED: bool = false;

/// Whether the generic body's screen is drawn at the dpi simee reports for the Android reference
/// profile (a TV is sized by its nominal diagonal, not the dpi the AVD reports; its lane may change this). When false, the shared test checks only the aspect.
pub(super) const SIZES_BY_DPI: bool = false;

/// The TV rows of the table (read only while [`ENABLED`]).
pub(super) fn rows() -> Vec<Body> {
    Vec::new()
}

/// A TV with no row (asked only while [`ENABLED`]).
pub(super) fn generic(_: &Spec) -> Option<Body> {
    None
}
