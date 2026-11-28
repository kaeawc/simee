//! Pure, dependency-light simee building blocks. No FFI, no I/O (`net` and `hostfs` name the
//! host's Unix sockets and positional file I/O, or stand in where it has none): everything here is unit-tested.

pub mod body;
pub mod carplay;
pub mod contrast;
pub mod debug;
pub mod device;
pub mod duo;
// Not on wasm (the website builds sim-core there; the browser has no OS entropy to give).
#[cfg(not(target_family = "wasm"))]
pub mod entropy;
pub mod fold;
pub mod form_factor;
pub mod frame;
pub mod framebuffer;
pub mod geometry;
pub mod gesture;
pub mod goldfish;
pub mod hid;
pub mod hostfs;
pub mod idle;
pub mod indigo;
pub mod launchd;
pub mod logcat;
pub mod mark;
pub mod net;
pub mod network;
pub mod pacing;
pub mod pixels;
pub mod platform;
pub mod plist;
pub mod pointer;
pub mod reuse;
pub mod rotation;
pub mod shot;
pub mod stream;
pub mod stylus;
pub mod support;
pub mod vhal;
pub mod worker;
pub mod xr;
pub mod yuv;
