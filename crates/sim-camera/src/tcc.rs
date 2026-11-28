//! The host camera permission. On macOS, camera access is a TCC grant held by the *responsible
//! app* of the capturing process: the app that started the simee server (Terminal, iTerm, an IDE,
//! or the launchd job's own bundle), not the `simee` binary. A capture without the grant gets no
//! frames (or black ones), so a webcam source checks the status first and fails with the fix.
//!
//! Only the status is read here ([`HostAuthorizer`]); [`request_access`] is the one call that can
//! show the prompt, and nothing calls it implicitly. Linux has no camera permission system: access
//! is the device node's file mode (`/dev/video*`, usually the `video` group).

use serde::Serialize;

use crate::error::{Error, Result};

/// The camera permission of this process (its responsible app, on macOS).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Authorization {
    Authorized,
    Denied,
    /// Never asked: the prompt hasn't been shown to the responsible app.
    NotDetermined,
    /// Blocked by a configuration profile or Screen Time; the user can't grant it.
    Restricted,
    /// The host has no camera permission (Linux).
    NotRequired,
}

impl Authorization {
    /// Maps `AVAuthorizationStatus` (`+[AVCaptureDevice authorizationStatusForMediaType:]`).
    pub fn from_av_status(raw: isize) -> Option<Authorization> {
        match raw {
            0 => Some(Authorization::NotDetermined),
            1 => Some(Authorization::Restricted),
            2 => Some(Authorization::Denied),
            3 => Some(Authorization::Authorized),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Authorization::Authorized => "authorized",
            Authorization::Denied => "denied",
            Authorization::NotDetermined => "not-determined",
            Authorization::Restricted => "restricted",
            Authorization::NotRequired => "not-required",
        }
    }

    /// Whether a capture can start.
    pub fn allows_capture(self) -> bool {
        matches!(self, Authorization::Authorized | Authorization::NotRequired)
    }

    /// What to do about a status that blocks capture.
    pub fn fix(self) -> Option<&'static str> {
        match self {
            Authorization::Authorized | Authorization::NotRequired => None,
            Authorization::Denied => Some(
                "camera access was denied to the app that started the simee server (its responsible app: the terminal, \
                 IDE or launchd job). Turn it on in System Settings > Privacy & Security > Camera, or run \
                 `tccutil reset Camera <that app's bundle id>` to be asked again, then restart the simee server",
            ),
            Authorization::NotDetermined => Some(
                "macOS hasn't asked the app that started the simee server (the terminal, IDE or launchd job) for camera \
                 access yet. Ask once from a logged-in session (`sim_camera::tcc::request_access`), allow it in the \
                 prompt, then restart the simee server; a capture never starts unasked",
            ),
            Authorization::Restricted => Some(
                "camera access is restricted on this Mac by a configuration profile or Screen Time, and can't be granted \
                 from here; lift the restriction (System Settings > Screen Time > Content & Privacy, or the MDM's \
                 Privacy Preferences Policy Control payload), then restart the simee server",
            ),
        }
    }
}

/// Where the permission status comes from: the host, or a fake in tests.
pub trait Authorizer: Send + Sync {
    fn status(&self) -> Result<Authorization>;
}

/// The host's camera permission, read without ever showing a prompt.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostAuthorizer;

impl Authorizer for HostAuthorizer {
    fn status(&self) -> Result<Authorization> {
        status()
    }
}

/// The host's camera permission (`authorizationStatusForMediaType:` on macOS; never prompts).
pub fn status() -> Result<Authorization> {
    #[cfg(target_os = "macos")]
    {
        crate::avf::authorization_status()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Authorization::NotRequired)
    }
}

/// Shows the camera prompt when the status is `not-determined` and waits for the answer (macOS);
/// returns the status otherwise. The prompt is attributed to this process's responsible app.
pub fn request_access() -> Result<Authorization> {
    #[cfg(target_os = "macos")]
    {
        crate::avf::request_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Authorization::NotRequired)
    }
}

/// Fails with [`Error::Permission`] unless capture is allowed.
pub fn require(auth: &dyn Authorizer) -> Result<()> {
    let status = auth.status()?;
    match status.fix() {
        None => Ok(()),
        Some(fix) => Err(Error::Permission { status, fix: fix.to_string() }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fixed permission status.
    pub(crate) struct Fixed(pub Authorization);

    impl Authorizer for Fixed {
        fn status(&self) -> Result<Authorization> {
            Ok(self.0)
        }
    }

    #[test]
    fn av_statuses_map_to_their_names() {
        let mapped: Vec<_> = (0..4).map(|raw| Authorization::from_av_status(raw).map(Authorization::as_str)).collect();
        assert_eq!(mapped, [Some("not-determined"), Some("restricted"), Some("denied"), Some("authorized")]);
        assert_eq!(Authorization::from_av_status(4), None);
        assert_eq!(Authorization::from_av_status(-1), None);
    }

    #[test]
    fn blocking_statuses_fail_with_their_fix() {
        for status in [Authorization::Denied, Authorization::NotDetermined, Authorization::Restricted] {
            let err = require(&Fixed(status)).expect_err("must fail");
            let Error::Permission { status: got, fix } = &err else { panic!("{err:?}") };
            assert_eq!(*got, status);
            assert!(fix.contains("simee server"), "{fix}");
            assert!(err.to_string().starts_with(&format!("camera permission is {}:", status.as_str())), "{err}");
        }
        let denied = require(&Fixed(Authorization::Denied)).expect_err("must fail").to_string();
        assert!(denied.contains("Privacy & Security > Camera") && denied.contains("tccutil reset Camera"), "{denied}");
    }

    #[test]
    fn authorized_and_not_required_allow_capture() {
        assert_eq!(require(&Fixed(Authorization::Authorized)), Ok(()));
        assert_eq!(require(&Fixed(Authorization::NotRequired)), Ok(()));
        assert!(!Authorization::Denied.allows_capture());
    }

    #[test]
    fn statuses_serialize_kebab_case() {
        assert_eq!(serde_json::to_string(&Authorization::NotDetermined).ok().as_deref(), Some("\"not-determined\""));
    }

    /// Reads the host status (never prompts) and prints it; passes whatever the answer.
    #[test]
    fn host_status_is_readable() {
        let status = status();
        println!("camera permission: {status:?}");
        if cfg!(not(target_os = "macos")) {
            assert_eq!(status, Ok(Authorization::NotRequired));
        }
    }
}
