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

/// The TV rows of the table (read only while [`ENABLED`]).
pub(super) fn rows() -> Vec<Body> {
    Vec::new()
}

/// A TV with no row (asked only while [`ENABLED`]).
pub(super) fn generic(_: &Spec) -> Option<Body> {
    None
}
