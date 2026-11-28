//! Form factors: phone, tablet, foldable, watch, tv, auto and vision, which platforms support each,
//! and what a device of each kind looks like (screen, density, shape, fold) and takes as input.
//!
//! A [`Kind`] is a validated (platform, form factor) pair; [`Kind::new`] would reject a combination
//! a platform doesn't offer with the reason, so callers never fall into a silent gap. Today every
//! pair exists: iOS's foldable is the iPhone Duo (iOS 27.1 runtimes on, [`crate::duo`]).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::platform::{AndroidOs, AppleOs, Platform, SubPlatform};

/// The shape of device a form factor describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormFactor {
    Phone,
    Tablet,
    Foldable,
    Watch,
    Tv,
    Auto,
    Vision,
}

impl FormFactor {
    pub const ALL: [FormFactor; 7] = [
        FormFactor::Phone,
        FormFactor::Tablet,
        FormFactor::Foldable,
        FormFactor::Watch,
        FormFactor::Tv,
        FormFactor::Auto,
        FormFactor::Vision,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Foldable => "foldable",
            Self::Watch => "watch",
            Self::Tv => "tv",
            Self::Auto => "auto",
            Self::Vision => "vision",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Phone => "Phone",
            Self::Tablet => "Tablet",
            Self::Foldable => "Foldable",
            Self::Watch => "Watch",
            Self::Tv => "TV",
            Self::Auto => "Auto",
            Self::Vision => "Vision",
        }
    }

    /// Parses a machine name (case-insensitive); `car` is accepted for `auto`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.to_ascii_lowercase();
        Self::ALL.into_iter().find(|f| f.as_str() == s).or((s == "car").then_some(Self::Auto))
    }
}

/// A (platform, form factor) pair that no platform offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsupported {
    pub platform: Platform,
    pub form_factor: FormFactor,
    pub reason: &'static str,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not supported on {}: {}", self.form_factor.as_str(), self.platform.label(), self.reason)
    }
}

impl std::error::Error for Unsupported {}

/// The sub-platform that offers `form_factor` on `platform`, or why none does.
pub fn sub_platform(platform: Platform, form_factor: FormFactor) -> Result<SubPlatform, Unsupported> {
    use FormFactor as F;
    match (platform, form_factor) {
        (Platform::Ios, F::Phone) => Ok(SubPlatform::Apple(AppleOs::Ios)),
        (Platform::Ios, F::Tablet) => Ok(SubPlatform::Apple(AppleOs::IpadOs)),
        (Platform::Ios, F::Watch) => Ok(SubPlatform::Apple(AppleOs::WatchOs)),
        (Platform::Ios, F::Tv) => Ok(SubPlatform::Apple(AppleOs::TvOs)),
        (Platform::Ios, F::Vision) => Ok(SubPlatform::Apple(AppleOs::VisionOs)),
        (Platform::Ios, F::Auto) => Ok(SubPlatform::Apple(AppleOs::CarPlay)),
        // The iPhone Duo (iOS 27.1 runtimes on): an iPhone, so iOS.
        (Platform::Ios, F::Foldable) => Ok(SubPlatform::Apple(AppleOs::Ios)),
        (Platform::Android, F::Phone | F::Tablet | F::Foldable) => Ok(SubPlatform::Android(AndroidOs::Mobile)),
        (Platform::Android, F::Watch) => Ok(SubPlatform::Android(AndroidOs::WearOs)),
        (Platform::Android, F::Tv) => Ok(SubPlatform::Android(AndroidOs::Tv)),
        (Platform::Android, F::Auto) => Ok(SubPlatform::Android(AndroidOs::Automotive)),
        (Platform::Android, F::Vision) => Ok(SubPlatform::Android(AndroidOs::Xr)),
    }
}

/// A validated (platform, form factor) pair with its sub-platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Kind {
    pub platform: Platform,
    pub form_factor: FormFactor,
    pub sub_platform: SubPlatform,
}

impl Kind {
    /// Validates the pair.
    pub fn new(platform: Platform, form_factor: FormFactor) -> Result<Kind, Unsupported> {
        Ok(Kind { platform, form_factor, sub_platform: sub_platform(platform, form_factor)? })
    }

    /// `iPadOS · Tablet`, `Wear OS · Watch`, `Android · Foldable`.
    pub fn label(&self) -> String {
        format!("{} · {}", self.sub_platform.label(), self.form_factor.label())
    }

    /// What the device takes as input, most important first.
    pub fn inputs(&self) -> &'static [InputMethod] {
        use InputMethod as I;
        match self.form_factor {
            FormFactor::Phone | FormFactor::Tablet => &[I::Touch, I::Keyboard, I::Buttons],
            FormFactor::Foldable => &[I::Touch, I::Keyboard, I::Buttons, I::Hinge],
            FormFactor::Watch => &[I::Touch, I::Rotary, I::Buttons],
            FormFactor::Tv => &[I::DPad, I::Keyboard],
            FormFactor::Auto => &[I::Touch, I::HeadUnit],
            FormFactor::Vision => &[I::Gaze, I::Pinch, I::Keyboard],
        }
    }

    /// Whether the device takes `input`.
    pub fn accepts(&self, input: InputMethod) -> bool {
        self.inputs().contains(&input)
    }

    /// The reference profile: what a new device of this kind gets unless told otherwise. Real
    /// devices report their own screen ([`Profile::with_screen`]).
    pub fn profile(&self) -> Profile {
        use FormFactor as F;
        let (width, height, density, shape) = match (self.platform, self.form_factor) {
            // simee's AVD defaults: small screens boot markedly faster.
            (Platform::Android, F::Phone) => (720, 1280, Density::Dpi(320), Shape::Rounded),
            (Platform::Android, F::Tablet) => (2560, 1600, Density::Dpi(320), Shape::Rounded),
            (Platform::Android, F::Foldable) => (2208, 1840, Density::Dpi(420), Shape::Rectangle),
            (Platform::Android, F::Watch) => (454, 454, Density::Dpi(320), Shape::Round),
            (Platform::Android, F::Tv) => (1920, 1080, Density::Dpi(320), Shape::Rectangle),
            (Platform::Android, F::Auto) => (1024, 768, Density::Dpi(160), Shape::Rectangle),
            // The SDK's `xr_headset_device` (landscape).
            (Platform::Android, F::Vision) => (2560, 2558, Density::Dpi(240), Shape::Rectangle),
            (Platform::Ios, F::Phone) => (1206, 2622, Density::Scale(3), Shape::Rounded),
            (Platform::Ios, F::Tablet) => (1668, 2420, Density::Scale(2), Shape::Rounded),
            (Platform::Ios, F::Watch) => (416, 496, Density::Scale(2), Shape::Rounded),
            (Platform::Ios, F::Tv) => (1920, 1080, Density::Scale(1), Shape::Rectangle),
            (Platform::Ios, F::Vision) => (2732, 2048, Density::Scale(2), Shape::Rectangle),
            (Platform::Ios, F::Auto) => (800, 480, Density::Scale(2), Shape::Rectangle),
            // The iPhone Duo unfolded: its inner panel as it lies open (`duo::INNER_PX` turned).
            (Platform::Ios, F::Foldable) => (crate::duo::INNER_PX.1, crate::duo::INNER_PX.0, Density::Scale(3), Shape::Rounded),
        };
        let fold = match (self.platform, self.form_factor) {
            // The Pixel Fold, open flat; its cover screen folded.
            (Platform::Android, F::Foldable) => Some(Fold { angle: 180.0, folded_width: 1080, folded_height: 2092 }),
            // The iPhone Duo boots closed, on its outer screen.
            (Platform::Ios, F::Foldable) => {
                let (w, h) = crate::duo::OUTER_PX;
                Some(Fold { angle: crate::duo::CLOSED as f32, folded_width: w, folded_height: h })
            }
            _ => None,
        };
        Profile { kind: *self, width, height, density, shape, fold, inputs: self.inputs().to_vec() }
    }

    /// The reference profile for a new device booting the system image tagged `image_tag`: an
    /// Android XR image tagged `ai-glasses` gets the SDK's display glasses (`ai_glasses_device`)
    /// rather than the headset.
    pub fn image_profile(&self, image_tag: &str) -> Profile {
        let profile = self.profile();
        if self.sub_platform == SubPlatform::Android(AndroidOs::Xr) && is_glasses_tag(image_tag) {
            let (w, h, dpi) = ANDROID_GLASSES_SCREEN;
            return profile.with_screen(w, h, Density::Dpi(dpi));
        }
        profile
    }
}

/// The SDK's display glasses (`ai_glasses_device`, landscape): width, height, dpi.
pub const ANDROID_GLASSES_SCREEN: (u32, u32, u32) = (450, 450, 160);

/// Android XR system image tags: headsets (`google-xr`, `android-xr`, `android-xr-v3-playstore`,
/// `android-xr-preview-playstore`) and glasses (`ai-glasses`).
pub fn is_xr_tag(tag: &str) -> bool {
    tag.starts_with("android-xr") || tag == "google-xr" || is_glasses_tag(tag)
}

/// The AI glasses system image tag (`system-images;android-36;ai-glasses;arm64-v8a`).
pub fn is_glasses_tag(tag: &str) -> bool {
    tag == "ai-glasses"
}

/// Screen density: Android reports dots per inch, Apple a point-to-pixel scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "unit", content = "value", rename_all = "lowercase")]
pub enum Density {
    Dpi(u32),
    Scale(u32),
}

/// The visible outline of the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    Rectangle,
    /// Rounded corners (phones, tablets, Apple Watch).
    Rounded,
    /// A circular screen (round Wear OS watches).
    Round,
}

/// How far open a foldable is, by hinge angle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Posture {
    Closed,
    HalfOpened,
    Opened,
}

impl Posture {
    /// The posture for a hinge angle, with the stock emulator's ranges for foldables
    /// (`hw.sensor.hinge_angles_posture_definitions = 0-30, 30-150, 150-180`).
    pub fn from_angle(angle: f32) -> Posture {
        match angle {
            a if a < 30.0 => Self::Closed,
            a if a < 150.0 => Self::HalfOpened,
            _ => Self::Opened,
        }
    }

    /// A representative hinge angle for this posture.
    pub fn angle(self) -> f32 {
        match self {
            Self::Closed => 0.0,
            Self::HalfOpened => 90.0,
            Self::Opened => 180.0,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "closed" => Some(Self::Closed),
            "half-opened" | "half" | "tabletop" => Some(Self::HalfOpened),
            "opened" | "open" | "flat" => Some(Self::Opened),
            _ => None,
        }
    }
}

/// A foldable's hinge: its angle (0 closed, 180 flat) and the outer screen used while folded.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fold {
    pub angle: f32,
    pub folded_width: u32,
    pub folded_height: u32,
}

impl Fold {
    pub fn posture(&self) -> Posture {
        Posture::from_angle(self.angle)
    }
}

/// How a user drives a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputMethod {
    Touch,
    Keyboard,
    /// Hardware buttons (home, power, volume, side).
    Buttons,
    /// The foldable hinge (fold, unfold, half-open).
    Hinge,
    /// Wear OS rotary input / the watchOS Digital Crown.
    Rotary,
    /// TV remote: D-pad and select (the Siri Remote on tvOS).
    #[serde(rename = "dpad")]
    DPad,
    /// Car head-unit controls: rotary controller and hardware keys.
    HeadUnit,
    /// Look at a target: visionOS gaze, Android XR eye tracking.
    Gaze,
    /// Pinch to select: visionOS, Android XR hand tracking.
    Pinch,
}

impl InputMethod {
    /// The name each platform uses for this input.
    pub fn label(self, platform: Platform) -> &'static str {
        match (self, platform) {
            (Self::Touch, _) => "Touch",
            (Self::Keyboard, _) => "Keyboard",
            (Self::Buttons, _) => "Hardware buttons",
            (Self::Hinge, _) => "Hinge",
            (Self::Rotary, Platform::Ios) => "Digital Crown",
            (Self::Rotary, Platform::Android) => "Rotary input",
            (Self::DPad, Platform::Ios) => "Siri Remote",
            (Self::DPad, Platform::Android) => "D-pad remote",
            (Self::HeadUnit, Platform::Ios) => "CarPlay head unit",
            (Self::HeadUnit, Platform::Android) => "Automotive head unit",
            (Self::Gaze, Platform::Ios) => "Gaze",
            (Self::Gaze, Platform::Android) => "Eye tracking",
            (Self::Pinch, Platform::Ios) => "Pinch",
            (Self::Pinch, Platform::Android) => "Hand tracking",
        }
    }
}

/// What a device looks like and takes as input. Sizes are pixels in the natural orientation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(flatten)]
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    pub density: Density,
    pub shape: Shape,
    /// Foldables only.
    pub fold: Option<Fold>,
    pub inputs: Vec<InputMethod>,
}

impl Profile {
    /// This profile with a real device's screen.
    pub fn with_screen(mut self, width: u32, height: u32, density: Density) -> Profile {
        (self.width, self.height, self.density) = (width, height, density);
        self
    }
}

/// Every form factor on every platform: the kind, or why the platform doesn't offer it.
pub fn matrix() -> Vec<(FormFactor, Platform, Result<Kind, Unsupported>)> {
    FormFactor::ALL.into_iter().flat_map(|f| Platform::ALL.map(|p| (f, p, Kind::new(p, f)))).collect()
}

fn last_segment(id: &str) -> String {
    id.rsplit('.').next().unwrap_or(id).to_ascii_lowercase()
}

/// An Apple device's kind, from its simulator device type and runtime identifiers
/// (`com.apple.CoreSimulator.SimDeviceType.iPad-Pro-11-inch-M4`,
/// `com.apple.CoreSimulator.SimRuntime.xrOS-2-0`). The device type decides (iPads run iOS
/// runtimes); the runtime is the fallback. CarPlay is never inferred: it is a display attached to
/// an iPhone, not a device type.
pub fn apple_kind(device_type_id: &str, runtime_id: &str) -> Option<Kind> {
    let ty = last_segment(device_type_id);
    let rt = last_segment(runtime_id);
    let form_factor = if crate::duo::folds(device_type_id) {
        FormFactor::Foldable
    } else if ty.starts_with("ipad") {
        FormFactor::Tablet
    } else if ty.starts_with("iphone") || ty.starts_with("ipod") {
        FormFactor::Phone
    } else if ty.starts_with("apple-watch") || rt.starts_with("watchos") {
        FormFactor::Watch
    } else if ty.starts_with("apple-tv") || rt.starts_with("tvos") {
        FormFactor::Tv
    } else if ty.starts_with("apple-vision") || rt.starts_with("xros") || rt.starts_with("visionos") {
        FormFactor::Vision
    } else if rt.starts_with("ios") {
        FormFactor::Phone
    } else {
        return None;
    };
    Kind::new(Platform::Ios, form_factor).ok()
}

/// An Apple device type's form factor, from its identifier and product family: the iPhone Duo
/// (`...SimDeviceType.iPhone-Duo`) is a foldable, other types follow their family.
pub fn apple_type_form_factor(device_type_id: &str, product_family: &str) -> Option<FormFactor> {
    if crate::duo::folds(device_type_id) { Some(FormFactor::Foldable) } else { apple_family_form_factor(product_family) }
}

/// An Apple device type's form factor, from its product family (`iPhone`, `Apple Watch`, ...).
pub fn apple_family_form_factor(product_family: &str) -> Option<FormFactor> {
    match product_family {
        "iPhone" | "iPod" => Some(FormFactor::Phone),
        "iPad" => Some(FormFactor::Tablet),
        "Apple Watch" => Some(FormFactor::Watch),
        "Apple TV" => Some(FormFactor::Tv),
        "Apple Vision" => Some(FormFactor::Vision),
        _ => None,
    }
}

/// An Android virtual device's profile, from its `config.ini` (`get` looks a key up). The system
/// image tag decides watch, TV, automotive and XR; a hinge sensor (or a `fold` device name) makes
/// a foldable; a smallest width of 600 dp or more (Android's own tablet threshold) makes a tablet.
pub fn android_profile(get: impl Fn(&str) -> Option<String>) -> Profile {
    let flag = |k: &str| get(k).is_some_and(|v| matches!(v.trim(), "yes" | "true"));
    let num = |k: &str, d: u32| get(k).and_then(|v| v.trim().parse().ok()).unwrap_or(d);
    let tags = format!("{},{}", get("tag.id").unwrap_or_default(), get("tag.ids").unwrap_or_default()).to_ascii_lowercase();
    // 16 KB page-size variants (`google-tv-ps16k`, `google_apis_ps16k`) count as their base tag.
    let tags: Vec<&str> = tags.split(',').map(|t| t.trim().trim_end_matches("-ps16k").trim_end_matches("_ps16k")).collect();
    let device = get("hw.device.name").unwrap_or_default().to_ascii_lowercase();
    let (w, h, dpi) = (num("hw.lcd.width", 1080), num("hw.lcd.height", 2400), num("hw.lcd.density", 420).max(1));
    let smallest_dp = w.min(h) * 160 / dpi;
    let form_factor = if tags.iter().any(|t| is_xr_tag(t)) {
        FormFactor::Vision
    } else if tags.iter().any(|t| t.contains("wear")) {
        FormFactor::Watch
    } else if tags.iter().any(|t| t.ends_with("-tv") || *t == "tv") {
        FormFactor::Tv
    } else if tags.iter().any(|t| t.contains("automotive")) {
        FormFactor::Auto
    } else if flag("hw.sensor.hinge") || device.contains("fold") {
        FormFactor::Foldable
    } else if device.contains("tablet") || smallest_dp >= 600 {
        FormFactor::Tablet
    } else {
        FormFactor::Phone
    };
    let kind = Kind::new(Platform::Android, form_factor).unwrap_or(Kind {
        platform: Platform::Android,
        form_factor: FormFactor::Phone,
        sub_platform: SubPlatform::Android(AndroidOs::Mobile),
    });
    let mut profile = kind.profile().with_screen(w, h, Density::Dpi(dpi));
    if form_factor == FormFactor::Watch {
        profile.shape = if device.contains("square") || device.contains("rect") { Shape::Rectangle } else { Shape::Round };
    }
    if let Some(fold) = &mut profile.fold {
        fold.angle = get("hw.sensor.hinge.defaults").and_then(|v| v.split(',').next()?.trim().parse().ok()).unwrap_or(180.0);
        if let Some(region) = crate::fold::Hinge::from_config(&get).and_then(|h| h.folded) {
            (fold.folded_width, fold.folded_height) = (region.width, region.height);
        }
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn every_supported_pair_has_the_right_sub_platform() {
        let expect = [
            (FormFactor::Phone, Platform::Ios, Some("ios")),
            (FormFactor::Tablet, Platform::Ios, Some("ipados")),
            (FormFactor::Foldable, Platform::Ios, Some("ios")),
            (FormFactor::Watch, Platform::Ios, Some("watchos")),
            (FormFactor::Tv, Platform::Ios, Some("tvos")),
            (FormFactor::Auto, Platform::Ios, Some("carplay")),
            (FormFactor::Vision, Platform::Ios, Some("visionos")),
            (FormFactor::Phone, Platform::Android, Some("android")),
            (FormFactor::Tablet, Platform::Android, Some("android")),
            (FormFactor::Foldable, Platform::Android, Some("android")),
            (FormFactor::Watch, Platform::Android, Some("wearos")),
            (FormFactor::Tv, Platform::Android, Some("androidtv")),
            (FormFactor::Auto, Platform::Android, Some("automotive")),
            (FormFactor::Vision, Platform::Android, Some("androidxr")),
        ];
        for (f, p, sub) in expect {
            assert_eq!(Kind::new(p, f).ok().map(|k| k.sub_platform.as_str()), sub, "{f:?} on {p:?}");
        }
        assert_eq!(matrix().len(), 14);
        // Every form factor exists on both platforms (the iPhone Duo is iOS's foldable).
        assert!(matrix().iter().all(|(.., k)| k.is_ok()));
        assert_eq!(Kind::new(Platform::Android, FormFactor::Vision).map(|k| k.label()), Ok("Android XR · Vision".to_owned()));
        assert_eq!(Kind::new(Platform::Ios, FormFactor::Foldable).map(|k| k.label()), Ok("iOS · Foldable".to_owned()));
    }

    #[test]
    fn unsupported_pairs_say_why() {
        let e = Unsupported { platform: Platform::Ios, form_factor: FormFactor::Auto, reason: "a reason" };
        assert_eq!(e.to_string(), "auto is not supported on iOS: a reason");
    }

    #[test]
    fn the_iphone_duo_is_ios_s_foldable() {
        let duo = Kind::new(Platform::Ios, FormFactor::Foldable).expect("duo").profile();
        assert_eq!((duo.width, duo.height, duo.density, duo.shape), (2853, 2007, Density::Scale(3), Shape::Rounded));
        let fold = duo.fold.expect("a hinge");
        assert_eq!((fold.folded_width, fold.folded_height, fold.posture()), (1398, 2034, Posture::Closed));
        assert!(duo.inputs.contains(&InputMethod::Hinge));
        let dt = "com.apple.CoreSimulator.SimDeviceType.";
        let k = apple_kind(&format!("{dt}iPhone-Duo"), "com.apple.CoreSimulator.SimRuntime.iOS-27-1").expect("kind");
        assert_eq!((k.form_factor, k.sub_platform.as_str()), (FormFactor::Foldable, "ios"));
        assert_eq!(apple_type_form_factor(&format!("{dt}iPhone-Duo"), "iPhone"), Some(FormFactor::Foldable));
        assert_eq!(apple_type_form_factor(&format!("{dt}iPhone-17"), "iPhone"), Some(FormFactor::Phone));
    }

    #[test]
    fn form_factor_names_round_trip() {
        for f in FormFactor::ALL {
            assert_eq!(FormFactor::parse(f.as_str()), Some(f));
            assert_eq!(serde_json::to_value(f).expect("json"), f.as_str());
        }
        assert_eq!(FormFactor::parse("CAR"), Some(FormFactor::Auto));
        assert_eq!(FormFactor::parse("laptop"), None);
    }

    #[test]
    fn profiles_carry_shape_fold_and_inputs() {
        let watch = Kind::new(Platform::Android, FormFactor::Watch).expect("wear").profile();
        assert_eq!((watch.shape, watch.density), (Shape::Round, Density::Dpi(320)));
        assert!(watch.inputs.contains(&InputMethod::Rotary));
        let fold = Kind::new(Platform::Android, FormFactor::Foldable).expect("fold").profile();
        assert_eq!(fold.fold.map(|f| f.posture()), Some(Posture::Opened));
        assert!(Kind::new(Platform::Android, FormFactor::Foldable).expect("fold").accepts(InputMethod::Hinge));
        assert!(!Kind::new(Platform::Ios, FormFactor::Phone).expect("phone").accepts(InputMethod::Hinge));
        let tv = Kind::new(Platform::Ios, FormFactor::Tv).expect("tv");
        assert_eq!(tv.inputs()[0].label(Platform::Ios), "Siri Remote");
        assert_eq!(InputMethod::Rotary.label(Platform::Ios), "Digital Crown");
        assert_eq!(Kind::new(Platform::Ios, FormFactor::Auto).expect("carplay").label(), "CarPlay · Auto");
        for (_, _, kind) in matrix() {
            let Ok(kind) = kind else { continue };
            let p = kind.profile();
            assert!(p.width > 0 && p.height > 0, "{kind:?}");
            assert_eq!(p.fold.is_some(), kind.form_factor == FormFactor::Foldable);
        }
        let json = serde_json::to_value(&fold).expect("json");
        assert_eq!(json["form_factor"], "foldable");
        assert_eq!(json["sub_platform"], "android");
        assert_eq!(json["density"], serde_json::json!({"unit": "dpi", "value": 420}));
        assert_eq!(json["inputs"][3], "hinge");
    }

    #[test]
    fn postures_follow_the_stock_hinge_ranges() {
        assert_eq!(Posture::from_angle(0.0), Posture::Closed);
        assert_eq!(Posture::from_angle(29.9), Posture::Closed);
        assert_eq!(Posture::from_angle(30.0), Posture::HalfOpened);
        assert_eq!(Posture::from_angle(149.0), Posture::HalfOpened);
        assert_eq!(Posture::from_angle(180.0), Posture::Opened);
        for p in [Posture::Closed, Posture::HalfOpened, Posture::Opened] {
            assert_eq!(Posture::from_angle(p.angle()), p);
        }
        assert_eq!(Posture::parse("tabletop"), Some(Posture::HalfOpened));
    }

    #[test]
    fn apple_kinds_come_from_device_type_then_runtime() {
        let k = |ty: &str, rt: &str| apple_kind(ty, rt).map(|k| (k.form_factor, k.sub_platform.as_str()));
        let dt = "com.apple.CoreSimulator.SimDeviceType.";
        let rt = "com.apple.CoreSimulator.SimRuntime.";
        assert_eq!(k(&format!("{dt}iPhone-17"), &format!("{rt}iOS-26-5")), Some((FormFactor::Phone, "ios")));
        assert_eq!(k(&format!("{dt}iPad-Pro-11-inch-M4"), &format!("{rt}iOS-26-5")), Some((FormFactor::Tablet, "ipados")));
        assert_eq!(k(&format!("{dt}Apple-Watch-Series-10-46mm"), &format!("{rt}watchOS-26-0")), Some((FormFactor::Watch, "watchos")));
        assert_eq!(k(&format!("{dt}Apple-TV-4K-3rd-generation-4K"), &format!("{rt}tvOS-26-0")), Some((FormFactor::Tv, "tvos")));
        assert_eq!(k(&format!("{dt}Apple-Vision-Pro"), &format!("{rt}xrOS-26-0")), Some((FormFactor::Vision, "visionos")));
        assert_eq!(k("custom", &format!("{rt}watchOS-11-0")), Some((FormFactor::Watch, "watchos")));
        assert_eq!(k("custom", "weird"), None);
        assert_eq!(apple_family_form_factor("Apple Vision"), Some(FormFactor::Vision));
        assert_eq!(apple_family_form_factor("Mac"), None);
    }

    fn avd(pairs: &[(&str, &str)]) -> Profile {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        android_profile(|k| map.get(k).cloned())
    }

    #[test]
    fn android_kinds_come_from_the_avd_config() {
        let phone = avd(&[("tag.id", "google_apis"), ("hw.lcd.width", "720"), ("hw.lcd.height", "1280"), ("hw.lcd.density", "320")]);
        assert_eq!((phone.kind.form_factor, phone.width, phone.density), (FormFactor::Phone, 720, Density::Dpi(320)));
        // 1600 px at 320 dpi is 800 dp: a tablet by Android's own threshold.
        let tablet = avd(&[("tag.id", "google_apis"), ("hw.lcd.width", "1600"), ("hw.lcd.height", "2560"), ("hw.lcd.density", "320")]);
        assert_eq!(tablet.kind.form_factor, FormFactor::Tablet);
        let fold = avd(&[("tag.id", "google_apis"), ("hw.sensor.hinge", "yes"), ("hw.sensor.hinge.defaults", "90")]);
        assert_eq!((fold.kind.form_factor, fold.fold.map(|f| f.posture())), (FormFactor::Foldable, Some(Posture::HalfOpened)));
        assert_eq!(avd(&[("hw.device.name", "pixel_fold")]).kind.form_factor, FormFactor::Foldable);
        let duo = avd(&[
            ("hw.sensor.hinge", "yes"),
            ("hw.displayRegion.0.1.xOffset", "0"),
            ("hw.displayRegion.0.1.yOffset", "0"),
            ("hw.displayRegion.0.1.width", "1350"),
            ("hw.displayRegion.0.1.height", "1800"),
        ]);
        assert_eq!(duo.fold.map(|f| (f.folded_width, f.folded_height)), Some((1350, 1800)), "the folded region comes from the AVD");
        let wear = avd(&[("tag.id", "android-wear"), ("hw.device.name", "wearos_large_round")]);
        assert_eq!((wear.kind.sub_platform.as_str(), wear.shape), ("wearos", Shape::Round));
        assert_eq!(avd(&[("tag.id", "android-wear"), ("hw.device.name", "wearos_square")]).shape, Shape::Rectangle);
        assert_eq!(avd(&[("tag.id", "google-tv")]).kind.sub_platform.as_str(), "androidtv");
        assert_eq!(avd(&[("tag.id", "android-tv")]).kind.form_factor, FormFactor::Tv);
        assert_eq!(avd(&[("tag.ids", "android-automotive-playstore")]).kind.sub_platform.as_str(), "automotive");
        assert_eq!(avd(&[]).kind.form_factor, FormFactor::Phone);
        // Tags auto-mobile recognises: signed and China Wear OS images, 16 KB page-size Google TV.
        assert_eq!(avd(&[("tag.id", "android-wear-signed")]).kind.form_factor, FormFactor::Watch);
        assert_eq!(avd(&[("tag.id", "android-wear-cn")]).kind.form_factor, FormFactor::Watch);
        assert_eq!(avd(&[("tag.id", "google-tv-ps16k")]).kind.form_factor, FormFactor::Tv);
        assert_eq!(avd(&[("tag.id", "google_apis_ps16k")]).kind.form_factor, FormFactor::Phone);
    }

    #[test]
    fn android_xr_images_are_vision_devices() {
        // What avdmanager writes for the SDK's XR headset, and the image directory tags simee and
        // simee write (`google-xr`, `android-xr-v3-playstore`, `android-xr-preview-playstore`).
        let headset = avd(&[
            ("tag.id", "android-xr"),
            ("tag.ids", "android-xr,google_apis_playstore"),
            ("hw.device.name", "xr_headset_device"),
            ("hw.lcd.width", "2560"),
            ("hw.lcd.height", "2558"),
            ("hw.lcd.density", "240"),
        ]);
        assert_eq!((headset.kind.form_factor, headset.kind.sub_platform.as_str()), (FormFactor::Vision, "androidxr"));
        assert_eq!((headset.width, headset.height, headset.density), (2560, 2558, Density::Dpi(240)));
        for tag in ["google-xr", "android-xr-v3-playstore", "android-xr-preview-playstore", "ai-glasses"] {
            assert_eq!(avd(&[("tag.id", tag)]).kind.label(), "Android XR · Vision", "{tag}");
        }
        assert!(!is_xr_tag("google_apis_playstore") && !is_glasses_tag("google-xr"));
    }

    #[test]
    fn android_xr_profiles_and_inputs() {
        let xr = Kind::new(Platform::Android, FormFactor::Vision).expect("Android XR");
        let headset = xr.profile();
        assert_eq!((headset.width, headset.height, headset.density, headset.shape), (2560, 2558, Density::Dpi(240), Shape::Rectangle));
        assert_eq!(xr.image_profile("google-xr"), headset);
        let glasses = xr.image_profile("ai-glasses");
        assert_eq!((glasses.width, glasses.height, glasses.density), (450, 450, Density::Dpi(160)));
        // Only Android XR has glasses; other kinds ignore the tag.
        let phone = Kind::new(Platform::Android, FormFactor::Phone).expect("phone");
        assert_eq!(phone.image_profile("ai-glasses"), phone.profile());
        let labels: Vec<&str> = xr.inputs().iter().map(|i| i.label(Platform::Android)).collect();
        assert_eq!(labels, ["Eye tracking", "Hand tracking", "Keyboard"]);
        let vision = Kind::new(Platform::Ios, FormFactor::Vision).expect("visionOS");
        assert_eq!(vision.inputs().iter().map(|i| i.label(Platform::Ios)).collect::<Vec<_>>(), ["Gaze", "Pinch", "Keyboard"]);
    }
}
