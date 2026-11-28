//! The source catalog: the host's cameras (real and virtual) and the synthetic kinds, plus the
//! hot-plug difference between two listings.

use serde::Serialize;

use crate::error::Result;

/// What a host camera is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraKind {
    /// The Mac's own camera.
    BuiltIn,
    /// A plugged-in camera (USB, Thunderbolt, a capture card).
    External,
    /// An iPhone used as a camera (Continuity Camera).
    Continuity,
    /// Continuity Camera's Desk View: a crop of the iPhone's ultra-wide camera.
    DeskView,
    /// Software that presents itself as a camera: a CoreMediaIO camera extension (OBS Virtual
    /// Camera, Camo, mmhmm) or v4l2loopback.
    Virtual,
    Unknown,
}

/// One format a camera offers.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    /// The pixel format as its four characters (`420v`, `YUYV`, `MJPG`).
    pub pixel_format: String,
    /// The highest frame rate at this size, when the host says.
    pub max_fps: Option<f64>,
}

/// A camera on the host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HostCamera {
    /// Stable across reboots and replugs: AVFoundation's `uniqueID` on macOS, the V4L2 bus info on
    /// Linux.
    pub id: String,
    pub name: String,
    pub kind: CameraKind,
    /// Whether the camera is software rather than a sensor.
    #[serde(rename = "virtual")]
    pub is_virtual: bool,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    /// How it is attached: AVFoundation's transport type (`bltn`, `usb `, `virt`) or the V4L2
    /// driver name.
    pub transport: Option<String>,
    /// The device node to open (Linux).
    pub path: Option<String>,
    pub formats: Vec<Format>,
}

impl HostCamera {
    /// The id to link it by: `webcam:<id>`.
    pub fn source_id(&self) -> String {
        format!("webcam:{}", self.id)
    }
}

/// A synthetic source kind, for listings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SyntheticKind {
    pub prefix: &'static str,
    pub example: &'static str,
    pub description: &'static str,
    /// Whether this build can serve it.
    pub supported: bool,
}

/// The synthetic kinds: no hardware needed.
pub const SYNTHETIC: &[SyntheticKind] = &[
    SyntheticKind {
        prefix: "pattern",
        example: "pattern:testcard",
        description: "colour bars, a moving marker, the frame number (also as a 32-bit strip) and the wall clock",
        supported: true,
    },
    SyntheticKind { prefix: "qr", example: "qr:hello", description: "a QR code of the text", supported: true },
    SyntheticKind { prefix: "text", example: "text:Hello", description: "the text, centred and wrapped", supported: true },
    SyntheticKind { prefix: "image", example: "image:/path/to/photo.jpg", description: "a PNG or JPEG still", supported: true },
    SyntheticKind {
        prefix: "video",
        example: "video:/path/to/clip.mp4",
        description: "a movie file's first video track, looped at its own frame rate (macOS: decoded with AVFoundation)",
        supported: cfg!(target_os = "macos"),
    },
    SyntheticKind { prefix: "solid", example: "solid:#336699", description: "one colour (name or hex)", supported: true },
];

/// The host's cameras, real and virtual, in the host's order. Listing never needs the camera
/// permission and never prompts; a host with no cameras gives an empty list.
pub fn host_cameras() -> Result<Vec<HostCamera>> {
    #[cfg(target_os = "macos")]
    {
        crate::avf::cameras()
    }
    #[cfg(target_os = "linux")]
    {
        crate::v4l2::cameras()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(Vec::new())
    }
}

/// A camera that appeared or went away between two listings.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Added(HostCamera),
    Removed(HostCamera),
}

/// The hot-plug events between two listings, by id: removals first, then additions.
pub fn changes(before: &[HostCamera], after: &[HostCamera]) -> Vec<Change> {
    let removed = before.iter().filter(|b| !after.iter().any(|a| a.id == b.id)).cloned().map(Change::Removed);
    let added = after.iter().filter(|a| !before.iter().any(|b| b.id == a.id)).cloned().map(Change::Added);
    removed.chain(added).collect()
}

/// Names, models and makers that mark a camera as software, for virtual cameras that don't say so
/// in their transport type.
const VIRTUAL_MARKERS: &[&str] = &["virtual", "obs", "camo", "mmhmm", "snap camera", "manycam", "xsplit", "ndi", "loopback"];

/// `'virt'`: `kIOAudioDeviceTransportTypeVirtual`, what CoreMediaIO camera extensions report.
pub const TRANSPORT_VIRTUAL: u32 = u32::from_be_bytes(*b"virt");
/// `'bltn'`: `kIOAudioDeviceTransportTypeBuiltIn`.
pub const TRANSPORT_BUILT_IN: u32 = u32::from_be_bytes(*b"bltn");

/// Classifies an AVFoundation camera from its device type, transport type and descriptive
/// strings: (kind, virtual).
pub fn classify_apple(device_type: &str, transport: u32, texts: &[&str]) -> (CameraKind, bool) {
    let marked = texts.iter().any(|t| {
        let t = t.to_ascii_lowercase();
        VIRTUAL_MARKERS.iter().any(|m| t.contains(m))
    });
    let kind = match device_type.strip_prefix("AVCaptureDeviceType").unwrap_or(device_type) {
        "BuiltInWideAngleCamera" if transport != TRANSPORT_VIRTUAL => CameraKind::BuiltIn,
        "ContinuityCamera" => CameraKind::Continuity,
        "DeskViewCamera" => CameraKind::DeskView,
        _ if transport == TRANSPORT_VIRTUAL || marked => CameraKind::Virtual,
        "External" | "ExternalUnknown" if transport == TRANSPORT_BUILT_IN => CameraKind::BuiltIn,
        "External" | "ExternalUnknown" => CameraKind::External,
        _ => CameraKind::Unknown,
    };
    (kind, kind == CameraKind::Virtual)
}

/// Classifies a V4L2 capture node from `VIDIOC_QUERYCAP`'s driver, card and bus info.
pub fn classify_v4l2(driver: &str, card: &str, bus_info: &str) -> (CameraKind, bool) {
    let driver_l = driver.to_ascii_lowercase();
    let software = ["v4l2 loopback", "v4l2loopback", "vivid", "akvcam"].iter().any(|d| driver_l == *d)
        || bus_info.starts_with("platform:v4l2loopback")
        || VIRTUAL_MARKERS.iter().any(|m| card.to_ascii_lowercase().contains(m));
    let kind = if software {
        CameraKind::Virtual
    } else if bus_info.starts_with("usb-") {
        CameraKind::External
    } else {
        CameraKind::Unknown
    };
    (kind, software)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn camera(id: &str, name: &str) -> HostCamera {
        HostCamera {
            id: id.into(),
            name: name.into(),
            kind: CameraKind::External,
            is_virtual: false,
            manufacturer: None,
            model: None,
            transport: None,
            path: None,
            formats: Vec::new(),
        }
    }

    #[test]
    fn apple_cameras_are_classified_by_type_and_transport() {
        let usb = u32::from_be_bytes(*b"usb ");
        let cases = [
            ("AVCaptureDeviceTypeBuiltInWideAngleCamera", TRANSPORT_BUILT_IN, "FaceTime HD Camera", CameraKind::BuiltIn),
            ("AVCaptureDeviceTypeContinuityCamera", 0, "iPhone Camera", CameraKind::Continuity),
            ("AVCaptureDeviceTypeDeskViewCamera", 0, "iPhone Desk View Camera", CameraKind::DeskView),
            ("AVCaptureDeviceTypeExternal", usb, "Logitech BRIO", CameraKind::External),
            ("AVCaptureDeviceTypeExternal", TRANSPORT_VIRTUAL, "Some Extension Cam", CameraKind::Virtual),
            ("AVCaptureDeviceTypeExternal", 0, "OBS Virtual Camera", CameraKind::Virtual),
            ("AVCaptureDeviceTypeExternalUnknown", 0, "Camo", CameraKind::Virtual),
            ("AVCaptureDeviceTypeExternal", TRANSPORT_BUILT_IN, "Studio Display Camera", CameraKind::BuiltIn),
            ("AVCaptureDeviceTypeSomethingNew", usb, "Mystery", CameraKind::Unknown),
        ];
        for (ty, transport, name, kind) in cases {
            assert_eq!(classify_apple(ty, transport, &[name]), (kind, kind == CameraKind::Virtual), "{name}");
        }
    }

    #[test]
    fn v4l2loopback_and_test_drivers_are_virtual() {
        assert_eq!(classify_v4l2("v4l2 loopback", "Dummy video device (0x0000)", "platform:v4l2loopback-000"), (CameraKind::Virtual, true));
        assert_eq!(classify_v4l2("vivid", "vivid", "platform:vivid-000"), (CameraKind::Virtual, true));
        assert_eq!(classify_v4l2("uvcvideo", "OBS Cam", "platform:x"), (CameraKind::Virtual, true));
        assert_eq!(classify_v4l2("uvcvideo", "HD Pro Webcam C920", "usb-0000:00:14.0-1"), (CameraKind::External, false));
        assert_eq!(classify_v4l2("ipu6", "Intel MIPI Camera", "PCI:0000:00:05.0"), (CameraKind::Unknown, false));
    }

    #[test]
    fn changes_report_removals_then_additions() {
        let a = camera("a", "A");
        let b = camera("b", "B");
        let c = camera("c", "C");
        let events = changes(&[a.clone(), b.clone()], &[b.clone(), c.clone()]);
        assert_eq!(events, vec![Change::Removed(a), Change::Added(c)]);
        let same = std::slice::from_ref(&b);
        assert!(changes(same, same).is_empty());
    }

    #[test]
    fn cameras_serialize_with_a_virtual_flag() {
        let json = serde_json::to_value(camera("id", "Cam")).unwrap_or_default();
        assert_eq!(json["virtual"], false);
        assert_eq!(json["kind"], "external");
        assert_eq!(camera("x", "y").source_id(), "webcam:x");
    }

    /// Lists the host's cameras (read-only: no permission needed, no prompt) and prints them.
    /// Passes with none.
    #[test]
    fn lists_host_cameras() {
        let cameras = host_cameras();
        let cameras = match cameras {
            Ok(c) => c,
            // A Linux host whose nodes the user can't open lists nothing rather than failing CI.
            Err(e @ crate::Error::Permission { .. }) => {
                println!("cameras: {e}");
                return;
            }
            Err(e) => panic!("listing failed: {e}"),
        };
        println!("{} host camera(s)", cameras.len());
        for c in &cameras {
            let tag = if c.is_virtual { "virtual" } else { "real" };
            let transport = c.transport.as_deref().unwrap_or("-");
            println!("  {} [{tag}, {:?}, transport {transport}] {} ({} formats)", c.name, c.kind, c.source_id(), c.formats.len());
            assert!(!c.id.is_empty() && !c.name.is_empty());
        }
        let mut ids: Vec<&str> = cameras.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), cameras.len(), "ids are unique");
    }
}
