//! The `Source` trait every camera source implements, and the opener that turns a source id into
//! one: synthetic kinds directly, a camera feed by connecting to it, webcams after resolving the id
//! and checking the permission. Another device's screen (`device:`) is captured by the simee server
//! (`sim-api` wraps this opener), never here, so emu, which links this crate, links neither device
//! stack.

use std::sync::Arc;

use crate::catalog::{self, HostCamera};
use crate::error::Result;
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::id::{SourceSpec, resolve_webcam};
use crate::synth;
use crate::tcc::{self, Authorizer, HostAuthorizer};

/// Something that produces camera frames. The hub calls [`Source::frame`] once per tick of the
/// capture, at the size and rate its subscribers asked for; the source draws, decodes or fetches
/// the latest frame and returns it at `spec`'s size. A source is dropped when its capture stops,
/// which releases any host camera it holds.
pub trait Source: Send {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame>;

    /// Whether the source draws its frames at whatever size it is asked for (a test card, a QR
    /// code, a still). The hub then renders once per distinct subscriber size instead of cropping
    /// one frame, which would cut a QR code's quiet zone or shift the test card's counter strip.
    /// A camera, whose frames come at its own size, leaves this `false`.
    fn synthetic(&self) -> bool {
        false
    }
}

/// Turns source ids into sources. The hub keys captures by [`Opener::canonical`], so two spellings
/// of one camera (`webcam:obs`, `webcam:<its id>`) share one capture.
pub trait Opener: Send + Sync {
    /// The id the capture is keyed by: a webcam query resolved to `webcam:<id>`, anything else as
    /// it is.
    fn canonical(&self, spec: &SourceSpec) -> Result<SourceSpec> {
        Ok(spec.clone())
    }

    /// Opens the source for a capture that starts at `spec`.
    fn open(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Box<dyn Source>>;
}

type CameraList = dyn Fn() -> Result<Vec<HostCamera>> + Send + Sync;

/// The opener for real use: synthetic sources, camera feeds, and the host's cameras behind the
/// permission check. A `device:` source is refused with who serves it.
pub struct HostOpener {
    authorizer: Arc<dyn Authorizer>,
    cameras: Box<CameraList>,
}

impl HostOpener {
    pub fn new() -> HostOpener {
        HostOpener::with(Arc::new(HostAuthorizer), Box::new(catalog::host_cameras))
    }

    /// An opener with its permission status and camera list supplied (fakes in tests).
    pub fn with(authorizer: Arc<dyn Authorizer>, cameras: Box<CameraList>) -> HostOpener {
        HostOpener { authorizer, cameras }
    }
}

impl Default for HostOpener {
    fn default() -> HostOpener {
        HostOpener::new()
    }
}

impl Opener for HostOpener {
    fn canonical(&self, spec: &SourceSpec) -> Result<SourceSpec> {
        match spec {
            SourceSpec::Webcam(query) => {
                let cameras = (self.cameras)()?;
                Ok(SourceSpec::Webcam(resolve_webcam(query, &cameras)?.id.clone()))
            }
            other => Ok(other.clone()),
        }
    }

    fn open(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Box<dyn Source>> {
        let query = match source {
            SourceSpec::Webcam(query) => query,
            SourceSpec::Device(device) => return Err(crate::device::served_by_simee(device)),
            // A feed needs no camera permission: whoever serves it captured the source.
            SourceSpec::Feed(path) => return open_feed(path, spec),
            other => return synth::open(other),
        };
        // The permission first: a capture without it would deliver black frames, or none.
        tcc::require(&*self.authorizer)?;
        let cameras = (self.cameras)()?;
        let camera = resolve_webcam(query, &cameras)?;
        open_webcam(camera, spec)
    }
}

#[cfg(unix)]
fn open_feed(path: &std::path::Path, spec: FrameSpec) -> Result<Box<dyn Source>> {
    crate::feed::open(path, spec)
}

#[cfg(not(unix))]
fn open_feed(path: &std::path::Path, _spec: FrameSpec) -> Result<Box<dyn Source>> {
    Err(crate::Error::Unsupported(format!("feed:{}: camera feeds need Unix sockets", path.display())))
}

#[cfg(target_os = "macos")]
fn open_webcam(camera: &HostCamera, spec: FrameSpec) -> Result<Box<dyn Source>> {
    Ok(Box::new(crate::avf::Webcam::open(camera, spec)?))
}

#[cfg(target_os = "linux")]
fn open_webcam(camera: &HostCamera, spec: FrameSpec) -> Result<Box<dyn Source>> {
    Ok(Box::new(crate::v4l2::open(camera, spec)?))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_webcam(camera: &HostCamera, _spec: FrameSpec) -> Result<Box<dyn Source>> {
    Err(crate::Error::Unsupported(format!("capturing from {} isn't supported on this host", camera.name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::catalog::tests::camera;
    use crate::tcc::Authorization;
    use crate::tcc::tests::Fixed;

    fn opener(status: Authorization) -> HostOpener {
        HostOpener::with(Arc::new(Fixed(status)), Box::new(|| Ok(vec![camera("uid-1", "OBS Virtual Camera")])))
    }

    #[test]
    fn webcam_ids_canonicalize_to_the_camera_id() {
        let o = opener(Authorization::Authorized);
        assert_eq!(o.canonical(&SourceSpec::Webcam("obs".into())), Ok(SourceSpec::Webcam("uid-1".into())));
        assert!(matches!(o.canonical(&SourceSpec::Webcam("brio".into())), Err(Error::NotFound { .. })));
        let qr = SourceSpec::Qr("x".into());
        assert_eq!(o.canonical(&qr), Ok(qr));
    }

    #[test]
    fn a_webcam_without_permission_fails_before_any_capture() {
        for status in [Authorization::Denied, Authorization::NotDetermined, Authorization::Restricted] {
            let err = opener(status).open(&SourceSpec::Webcam("obs".into()), FrameSpec::default()).err();
            assert!(matches!(&err, Some(Error::Permission { status: s, .. }) if *s == status), "{err:?}");
        }
    }

    #[test]
    fn synthetic_sources_need_no_permission() {
        let o = opener(Authorization::Denied);
        let mut source = o.open(&SourceSpec::parse("solid:red").unwrap_or(SourceSpec::Qr("x".into())), FrameSpec::default());
        let frame = source.as_mut().map(|s| s.frame(FrameSpec::default(), Tick::start()));
        assert!(matches!(frame, Ok(Ok(_))));
    }
}
