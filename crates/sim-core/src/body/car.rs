//! Car head-unit bodies (Android Automotive, CarPlay): none yet.
//!
//! The form factor's module, as [`super::table`] asks of each: [`ENABLED`], [`rows`] and
//! [`generic`]. A head unit (housing, a key strip as `Role::Button` [`super::Block`]s, a rotary
//! `Role::Knob`, a `Role::Trim` dash surround) goes here, nowhere else: set [`ENABLED`] and the
//! table, [`super::has_body`], the mesh checks and the desktop's face-on test take it up. The
//! desktop's `carplay` scene shows the device type `CarPlay-800x480`.

use super::Body;
use super::table::Spec;

/// Car head units have no body yet ([`super::has_body`]).
pub(super) const ENABLED: bool = false;

/// The head-unit rows of the table (read only while [`ENABLED`]).
pub(super) fn rows() -> Vec<Body> {
    Vec::new()
}

/// A head unit with no row (asked only while [`ENABLED`]).
pub(super) fn generic(_: &Spec) -> Option<Body> {
    None
}
