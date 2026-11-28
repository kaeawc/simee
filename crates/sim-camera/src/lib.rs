//! Camera sources for simee devices: the host's cameras (built-in, external, Continuity Camera,
//! and virtual cameras: CoreMediaIO camera extensions such as OBS Virtual Camera on macOS,
//! v4l2loopback on Linux), still images and synthetic patterns, under one source id scheme.
//!
//! - [`catalog`] lists the host's cameras (never needs the camera permission) and the synthetic
//!   kinds, and diffs two listings into hot-plug events.
//! - [`id`] parses source ids (`webcam:<id or name>`, `pattern:testcard`, `qr:<text>`, …) and
//!   resolves a webcam query by id, name, index or part of the name.
//! - [`Source`] produces BGRA frames at a requested size; [`Hub`] runs one capture per source and
//!   fans it out to every subscriber, stopping it when the last one goes. A webcam opens in the
//!   mode nearest the request (size, then frame rate): AVFoundation on macOS, V4L2 streaming
//!   (YUYV and Motion JPEG to BGRA) on Linux.
//! - [`hotplug`]: [`Hub::events`] streams cameras added and removed (host notifications, debounced)
//!   and captures whose source went missing.
//! - [`device`]: another simee device's live screen as a camera (`device:<udid or name>`,
//!   optionally cropped), paced by that device's frames: the id, the crop and the source over a
//!   [`device::Screen`]. Reading a device's screen links its platform's stack, so the readers live
//!   in `sim-api` (the simee server), never here: emu links this crate and must link neither.
//! - [`feed`] serves one device camera's linked source over a Unix socket (BGRA frames with their
//!   size, number and link generation, paced by the source, per reader): to the capture shim
//!   injected into an iOS simulator's apps (`shims/ios-camera`), and to emu's camera HAL, which
//!   reads it as a `feed:<socket>` source ([`feed::FeedSource`]).
//! - `video:<path>` plays a movie file looped at its own frame rate (`video.rs` is the timeline,
//!   AVFoundation's `AVAssetReader` the decoder; macOS only).
//! - [`tcc`] reads the host camera permission (never prompting) and turns a missing grant into an
//!   error that names the fix, instead of black frames.
//!
//! Frames are plain BGRA buffers shared through an `Arc`. A consumer that needs an IOSurface (a
//! VideoToolbox encoder, the iOS feed) copies one in with `sim_coresim::videotoolbox`'s
//! `PixelBufferPool::fill`, the one copy on that path; this crate doesn't link VideoToolbox.

#[cfg(target_os = "macos")]
mod avf;
pub mod catalog;
#[cfg(any(target_os = "linux", test))]
mod convert;
pub mod device;
pub mod draw;
mod error;
#[cfg(unix)]
pub mod feed;
pub mod frame;
pub mod hotplug;
pub mod hub;
pub mod id;
/// The feed's name from when only iOS read it.
#[cfg(unix)]
pub use feed as ios_feed;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod select;
pub mod source;
#[cfg(any(target_os = "linux", test))]
mod stream;
pub mod synth;
pub mod tcc;
#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(any(target_os = "macos", test))]
mod video;

pub use catalog::{CameraKind, Change, Format, HostCamera, host_cameras};
pub use device::{DeviceSpec, Region};
pub use error::{Error, Result};
pub use frame::{CameraFrame, FrameSpec, Tick};
pub use hotplug::{Event, HostHotPlug, HotPlug, Signals};
pub use hub::{Events, Hub, Subscription};
pub use id::{Pattern, SourceSpec, resolve_webcam};
pub use source::{HostOpener, Opener, Source};
pub use tcc::Authorization;
