//! The simee mark: a cut emerald, two pyramids joined at a girdle.
//!
//! One description of the stone's shape ([`Cut`]), light ([`Style`], [`Tone`]) and motion
//! ([`Motion`]) that every surface draws from. The desktop app projects it live into Rune fills
//! (the activity indicator, the device list, the empty stage); `examples/brand.rs` rasterizes and
//! writes the app icon, `.icns`, favicon, menu bar template, PNGs and SVGs in `assets/brand/`.
//! To change the mark, edit [`Cut::SIMEE`], [`Style::MARK`] or a [`Tone`] ramp and run
//! `scripts/brand.sh`; every asset and the app follow.
//!
//! The cutter's terms name the parts: the crown is the upper pyramid, the girdle the widest ring
//! where the two meet, the pavilion the lower pyramid, a table a flat top and a culet a flat tip.
//! `docs/design/brand.md` maps the explorer's parameters (`docs/design/mark-explorer.html`) onto
//! these fields.

pub mod assets;
mod cut;
mod motion;
pub mod raster;
mod render;

pub use cut::{Cut, Solid};
pub use motion::Motion;
pub use render::{Facet, Finish, Pose, Style, Tone, render};
