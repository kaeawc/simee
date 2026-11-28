//! Safe API over the host simulator service.
//!
//! The service's frameworks are private and loaded at runtime; every call goes through the
//! Objective-C runtime by selector, so a missing class or method surfaces as [`Error::Unavailable`]
//! instead of a link failure. All types here are main-thread-agnostic but not `Send`: keep a
//! [`Context`] and everything derived from it on one thread.
//!
//! The service exists only on macOS, so elsewhere this crate is empty (like `emu-hvf`): callers
//! gate their iOS backend on `target_os = "macos"` and the shared core still builds and tests.
#![cfg(target_os = "macos")]

mod carplay;
mod display;
mod error;
mod hid;
pub mod host;
mod pair;
pub mod procs;
mod rt;
mod service;
pub mod surface_port;
#[cfg(feature = "videotoolbox")]
pub mod videotoolbox;
mod watch;

pub use display::{Display, FrameWatch, Surface};
pub use error::{Error, Result};
pub use hid::{Hid, track_sends, wait_sent};
pub use service::{Appearance, BootOptions, Context, Device, ExecOutput, StateWatch, StatusBar, developer_dir};
pub use sim_core::frame::Frame;
pub use sim_core::hid::{Direction, Orientation, WatchGesture};
pub use surface_port::{SurfaceInbox, SurfaceOutbox};
pub use watch::FileWatch;

/// Runs `f` in an autorelease pool. Long-lived threads that call the service (API input workers,
/// the desktop's capture) wrap each unit of work, so objects the service autoreleases are freed as
/// they go instead of when the thread exits.
pub fn autoreleasepool<R>(f: impl FnOnce() -> R) -> R {
    objc2::rc::autoreleasepool(|_| f())
}
