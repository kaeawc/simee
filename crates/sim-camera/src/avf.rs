//! macOS cameras through AVFoundation, loaded on first use: `AVCaptureDeviceDiscoverySession`
//! lists built-in, external, Continuity and Desk View cameras and every CoreMediaIO camera
//! extension (OBS Virtual Camera, Camo, mmhmm); `authorizationStatusForMediaType:` reads the
//! camera permission without prompting; `Webcam` captures BGRA frames through an
//! `AVCaptureSession` and a video data output, in the device format nearest the request with its
//! frame duration fixed; `watch` turns `AVCaptureDeviceWasConnected` / `WasDisconnected`
//! notifications into hot-plug signals; `VideoReader` decodes a `video:` file's first video track
//! to BGRA with an `AVAssetReader`, for `video::Player` to loop.
//!
//! Classes and constants are looked up at runtime, so an older macOS without one (Continuity
//! Camera's device type is macOS 14+) skips it instead of failing to load.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::encode::{Encode, Encoding};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject, NSObjectProtocol, Sel};
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_foundation::{NSError, NSString};

use crate::catalog::{Format, HostCamera, classify_apple};
use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::hotplug::Signals;
use crate::select::{self, Interval, Mode, Rates};
use crate::source::Source;
use crate::tcc::Authorization;
use crate::video::{Decode, Sample};

type Obj = Retained<AnyObject>;

const AVFOUNDATION: &CStr = c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation";
const COREMEDIA: &CStr = c"/System/Library/Frameworks/CoreMedia.framework/CoreMedia";
const COREMEDIAIO: &CStr = c"/System/Library/Frameworks/CoreMediaIO.framework/CoreMediaIO";
const COREVIDEO: &CStr = c"/System/Library/Frameworks/CoreVideo.framework/CoreVideo";
const COREFOUNDATION: &CStr = c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";

/// The device types to discover, newest first; each is skipped when this macOS lacks it.
/// `ExternalUnknown` is the pre-macOS 14 name of `External`.
const DEVICE_TYPES: &[&CStr] = &[
    c"AVCaptureDeviceTypeBuiltInWideAngleCamera",
    c"AVCaptureDeviceTypeExternal",
    c"AVCaptureDeviceTypeContinuityCamera",
    c"AVCaptureDeviceTypeDeskViewCamera",
];
const EXTERNAL_UNKNOWN: &CStr = c"AVCaptureDeviceTypeExternalUnknown";

const FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const LOCK_READ_ONLY: u64 = 1;
/// How long a capture waits for its first frame, or tolerates no new one, before it reports why.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

/// `CMTime`: `value / timescale` seconds.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// `kCMTimeFlags_Valid`.
const TIME_VALID: u32 = 1;

// SAFETY: the layout above is CoreMedia's `CMTime` (`{?=qiIq}`), an anonymous struct of int64_t,
// int32_t, uint32_t, int64_t.
unsafe impl Encode for CMTime {
    const ENCODING: Encoding = Encoding::Struct("?", &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING]);
}

impl CMTime {
    /// The duration as an interval; `None` for an invalid, zero or negative time.
    fn interval(self) -> Option<Interval> {
        if self.flags & TIME_VALID == 0 || self.value <= 0 || self.timescale <= 0 {
            return None;
        }
        Some(Interval { num: self.value as u64, den: self.timescale as u64 })
    }

    fn of(interval: Interval) -> Option<CMTime> {
        let value = i64::try_from(interval.num).ok().filter(|&v| v > 0)?;
        let timescale = i32::try_from(interval.den).ok().filter(|&t| t > 0)?;
        Some(CMTime { value, timescale, flags: TIME_VALID, epoch: 0 })
    }
}

/// `CGAffineTransform`: `(x, y)` maps to `(a x + c y + tx, b x + d y + ty)`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Transform {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    tx: f64,
    ty: f64,
}

// SAFETY: the layout above is CoreGraphics' `CGAffineTransform` on 64-bit targets
// (`{CGAffineTransform=dddddd}`): six CGFloat (double) fields in this order.
unsafe impl Encode for Transform {
    const ENCODING: Encoding = Encoding::Struct("CGAffineTransform", &[f64::ENCODING; 6]);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Dimensions {
    width: i32,
    height: i32,
}

/// The C functions and constants capture needs, resolved once. Constants are `NSString *`
/// (toll-free bridged `CFStringRef`) values, which live as long as the process; held as addresses
/// so the table is `Sync`.
struct Api {
    video: usize,
    device_types: Vec<usize>,
    /// `AVCaptureDeviceWasConnectedNotification` and `WasDisconnectedNotification`.
    plugged: Vec<usize>,
    pixel_format_key: usize,
    format_dimensions: unsafe extern "C" fn(*const c_void) -> Dimensions,
    format_subtype: unsafe extern "C" fn(*const c_void) -> u32,
    sample_image: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    lock: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    unlock: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    base: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    bytes_per_row: unsafe extern "C" fn(*mut c_void) -> usize,
    width: unsafe extern "C" fn(*mut c_void) -> usize,
    height: unsafe extern "C" fn(*mut c_void) -> usize,
    pixel_format: unsafe extern "C" fn(*mut c_void) -> u32,
    presentation_time: unsafe extern "C" fn(*mut c_void) -> CMTime,
    release: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn open(path: &CStr) -> Result<NonNull<c_void>> {
    // SAFETY: `path` is NUL-terminated; dlopen has no other preconditions.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW) };
    NonNull::new(handle).ok_or_else(|| Error::Unavailable(format!("can't load {}", path.to_string_lossy())))
}

fn symbol(lib: NonNull<c_void>, name: &CStr) -> Option<NonNull<c_void>> {
    // SAFETY: the handle came from a successful dlopen and `name` is NUL-terminated.
    NonNull::new(unsafe { libc::dlsym(lib.as_ptr(), name.as_ptr()) })
}

/// The value of an exported `NSString *const` (or `CFStringRef`) constant.
fn constant(lib: NonNull<c_void>, name: &CStr) -> Option<usize> {
    let sym = symbol(lib, name)?;
    // SAFETY: callers name pointer-sized string constants; read the pointer the symbol holds.
    let value = unsafe { sym.as_ptr().cast::<*const c_void>().read() };
    (!value.is_null()).then_some(value as usize)
}

/// A C function, as the function pointer type `F`.
fn function<F: Copy>(lib: NonNull<c_void>, name: &CStr) -> Result<F> {
    let sym = symbol(lib, name).ok_or_else(|| Error::Unavailable(format!("symbol {}", name.to_string_lossy())))?;
    assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
    // SAFETY: `F` is the `extern "C"` signature of the named function (each call site spells it
    // out), and function pointers are pointer-sized on Apple targets.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&sym.as_ptr()) })
}

fn load() -> Result<Api> {
    let av = open(AVFOUNDATION)?;
    let cm = open(COREMEDIA)?;
    let cv = open(COREVIDEO)?;
    let mut device_types: Vec<usize> = DEVICE_TYPES.iter().filter_map(|name| constant(av, name)).collect();
    if constant(av, c"AVCaptureDeviceTypeExternal").is_none() {
        device_types.extend(constant(av, EXTERNAL_UNKNOWN));
    }
    Ok(Api {
        video: constant(av, c"AVMediaTypeVideo").ok_or_else(|| Error::Unavailable("AVMediaTypeVideo".into()))?,
        device_types,
        plugged: [c"AVCaptureDeviceWasConnectedNotification", c"AVCaptureDeviceWasDisconnectedNotification"]
            .iter()
            .filter_map(|name| constant(av, name))
            .collect(),
        pixel_format_key: constant(cv, c"kCVPixelBufferPixelFormatTypeKey")
            .ok_or_else(|| Error::Unavailable("kCVPixelBufferPixelFormatTypeKey".into()))?,
        format_dimensions: function(cm, c"CMVideoFormatDescriptionGetDimensions")?,
        format_subtype: function(cm, c"CMFormatDescriptionGetMediaSubType")?,
        sample_image: function(cm, c"CMSampleBufferGetImageBuffer")?,
        lock: function(cv, c"CVPixelBufferLockBaseAddress")?,
        unlock: function(cv, c"CVPixelBufferUnlockBaseAddress")?,
        base: function(cv, c"CVPixelBufferGetBaseAddress")?,
        bytes_per_row: function(cv, c"CVPixelBufferGetBytesPerRow")?,
        width: function(cv, c"CVPixelBufferGetWidth")?,
        height: function(cv, c"CVPixelBufferGetHeight")?,
        pixel_format: function(cv, c"CVPixelBufferGetPixelFormatType")?,
        presentation_time: function(cm, c"CMSampleBufferGetPresentationTimeStamp")?,
        release: function(open(COREFOUNDATION)?, c"CFRelease")?,
    })
}

/// A file URL for `path`.
fn file_url(path: &Path) -> Result<Obj> {
    let text = NSString::from_str(&path.to_string_lossy());
    // SAFETY: `+fileURLWithPath:` takes a non-nil NSString and returns a URL.
    let url: Option<Obj> = unsafe { msg_send![class(c"NSURL")?, fileURLWithPath: &*text] };
    url.ok_or_else(|| Error::Invalid(format!("{} isn't a file path", path.display())))
}

/// An NSError out-parameter (or `error` property) as text.
fn describe_error(error: *mut NSError) -> String {
    // SAFETY: nil, or an NSError the call just returned (autoreleased, alive in the current pool).
    unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "no reason given".into())
}

/// A constant's object.
fn object(address: usize) -> &'static AnyObject {
    // SAFETY: `address` is a constant string read by `load`, valid for the process' lifetime.
    unsafe { &*(address as *const AnyObject) }
}

fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).ok_or_else(|| Error::Unavailable(format!("class {}", name.to_string_lossy())))
}

fn responds(obj: &AnyObject, sel: Sel) -> bool {
    // SAFETY: every NSObject implements `respondsToSelector:` returning BOOL.
    unsafe { msg_send![obj, respondsToSelector: sel] }
}

/// A string property; `None` when missing or nil.
fn string(obj: &AnyObject, sel: Sel) -> Option<String> {
    if !responds(obj, sel) {
        return None;
    }
    // SAFETY: callers pass getters returning `NSString *`, nil or not.
    let value: Option<Obj> = unsafe { msg_send![obj, performSelector: sel] };
    let s = value?.downcast::<NSString>().ok()?.to_string();
    (!s.is_empty()).then_some(s)
}

/// Sends a no-argument message whose return is a scalar or a CF pointer, through `objc_msgSend`
/// directly: objc2's checked `msg_send!` rejects CF types (`^{opaqueCMFormatDescription=}`) as
/// `*const c_void`.
///
/// # Safety
/// `sel` must name a no-argument method of `obj` whose return type is `T`.
unsafe fn send<T: Copy>(obj: &AnyObject, sel: Sel) -> T {
    type Getter<T> = unsafe extern "C" fn(*const AnyObject, Sel) -> T;
    // SAFETY: `objc_msgSend` is called through the target method's signature (the caller's
    // contract).
    let f: Getter<T> = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
    // SAFETY: `obj` is a valid object and responds to `sel` (the caller's contract).
    unsafe { f(obj, sel) }
}

fn array(array: &AnyObject) -> Vec<Obj> {
    // SAFETY: callers pass NSArray instances; `count` returns NSUInteger.
    let count: usize = unsafe { msg_send![array, count] };
    // SAFETY: indices are in bounds; `objectAtIndex:` returns a non-nil element.
    (0..count).map(|i| unsafe { msg_send![array, objectAtIndex: i] }).collect()
}

fn fourcc(code: u32) -> String {
    code.to_be_bytes().iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect()
}

pub(crate) fn authorization_status() -> Result<Authorization> {
    // The calls autorelease objects: drain them here, not when this (long-lived) thread exits.
    autoreleasepool(|_| authorization_status_inner())
}

fn authorization_status_inner() -> Result<Authorization> {
    let api = api()?;
    let device = class(c"AVCaptureDevice")?;
    // SAFETY: `+authorizationStatusForMediaType:` takes an AVMediaType string and returns an
    // AVAuthorizationStatus (NSInteger); it never prompts.
    let raw: isize = unsafe { msg_send![device, authorizationStatusForMediaType: object(api.video)] };
    Authorization::from_av_status(raw).ok_or_else(|| Error::Unavailable(format!("unknown AVAuthorizationStatus {raw}")))
}

pub(crate) fn request_access() -> Result<Authorization> {
    let status = authorization_status()?;
    if status != Authorization::NotDetermined {
        return Ok(status);
    }
    let api = api()?;
    let device = class(c"AVCaptureDevice")?;
    let (tx, rx) = mpsc::channel::<bool>();
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: `+requestAccessForMediaType:completionHandler:` takes an AVMediaType and a
    // `void (^)(BOOL)` block, which it copies and calls once on an arbitrary queue.
    let _: () = unsafe { msg_send![device, requestAccessForMediaType: object(api.video), completionHandler: &*block] };
    rx.recv_timeout(Duration::from_secs(600)).map_err(|_| Error::Capture("no answer to the camera prompt".into()))?;
    authorization_status()
}

pub(crate) fn cameras() -> Result<Vec<HostCamera>> {
    let api = api()?;
    let discovery = class(c"AVCaptureDeviceDiscoverySession")?;
    autoreleasepool(|_| {
        // SAFETY: `+[NSMutableArray array]` returns a new empty mutable array.
        let types: Option<Obj> = unsafe { msg_send![class(c"NSMutableArray")?, array] };
        let types = types.ok_or_else(|| Error::Unavailable("NSMutableArray".into()))?;
        for &t in &api.device_types {
            // SAFETY: `addObject:` takes a non-nil object (a device type constant).
            let _: () = unsafe { msg_send![&*types, addObject: object(t)] };
        }
        // SAFETY: takes an NSArray of device types, a media type and a position
        // (AVCaptureDevicePositionUnspecified, 0); returns a session or nil.
        let session: Option<Obj> =
            unsafe { msg_send![discovery, discoverySessionWithDeviceTypes: &*types, mediaType: object(api.video), position: 0isize] };
        let Some(session) = session else { return Ok(Vec::new()) };
        // SAFETY: `devices` returns an NSArray of AVCaptureDevice.
        let devices: Option<Obj> = unsafe { msg_send![&*session, devices] };
        Ok(devices.map(|d| array(&d)).unwrap_or_default().iter().filter_map(|d| describe(api, d)).collect())
    })
}

fn describe(api: &Api, device: &AnyObject) -> Option<HostCamera> {
    let id = string(device, sel!(uniqueID))?;
    let name = string(device, sel!(localizedName)).unwrap_or_else(|| id.clone());
    let manufacturer = string(device, sel!(manufacturer));
    let model = string(device, sel!(modelID));
    let device_type = string(device, sel!(deviceType)).unwrap_or_default();
    // SAFETY: `transportType` is a no-argument getter returning int32_t (checked to exist).
    let transport = responds(device, sel!(transportType)).then(|| unsafe { send::<i32>(device, sel!(transportType)) } as u32);
    let texts = [name.as_str(), manufacturer.as_deref().unwrap_or(""), model.as_deref().unwrap_or("")];
    let (kind, is_virtual) = classify_apple(&device_type, transport.unwrap_or(0), &texts);
    Some(HostCamera {
        id,
        name,
        kind,
        is_virtual,
        manufacturer,
        model,
        transport: transport.filter(|&t| t != 0).map(fourcc),
        path: None,
        formats: formats(api, device),
    })
}

fn formats(api: &Api, device: &AnyObject) -> Vec<Format> {
    if !responds(device, sel!(formats)) {
        return Vec::new();
    }
    // SAFETY: `formats` returns an NSArray of AVCaptureDeviceFormat.
    let list: Option<Obj> = unsafe { msg_send![device, formats] };
    let mut out: Vec<Format> = Vec::new();
    for format in list.map(|l| array(&l)).unwrap_or_default() {
        if !responds(&format, sel!(formatDescription)) {
            continue;
        }
        // SAFETY: `formatDescription` returns a CMFormatDescriptionRef owned by the format.
        let desc: *const c_void = unsafe { send(&format, sel!(formatDescription)) };
        if desc.is_null() {
            continue;
        }
        // SAFETY: `desc` is a live video format description (the format retains it).
        let (dims, subtype) = unsafe { ((api.format_dimensions)(desc), (api.format_subtype)(desc)) };
        let max_fps = max_fps(&format);
        let (width, height) = (dims.width.max(0) as u32, dims.height.max(0) as u32);
        let pixel_format = fourcc(subtype);
        match out.iter_mut().find(|f| (f.width, f.height, f.pixel_format.as_str()) == (width, height, pixel_format.as_str())) {
            Some(f) => f.max_fps = f.max_fps.into_iter().chain(max_fps).reduce(f64::max),
            None => out.push(Format { width, height, pixel_format, max_fps }),
        }
    }
    out
}

fn max_fps(format: &AnyObject) -> Option<f64> {
    if !responds(format, sel!(videoSupportedFrameRateRanges)) {
        return None;
    }
    // SAFETY: returns an NSArray of AVFrameRateRange.
    let ranges: Option<Obj> = unsafe { msg_send![format, videoSupportedFrameRateRanges] };
    // SAFETY: `maxFrameRate` returns Float64.
    ranges.map(|r| array(&r)).unwrap_or_default().iter().map(|r| unsafe { msg_send![&**r, maxFrameRate] }).reduce(f64::max)
}

/// A device's formats, each as a mode for [`select::nearest`]: its dimensions and the frame
/// durations of its `videoSupportedFrameRateRanges`, exactly as the device states them.
fn modes(api: &Api, device: &AnyObject) -> (Vec<Obj>, Vec<Mode>) {
    if !responds(device, sel!(formats)) {
        return (Vec::new(), Vec::new());
    }
    // SAFETY: `formats` returns an NSArray of AVCaptureDeviceFormat.
    let list: Option<Obj> = unsafe { msg_send![device, formats] };
    let formats = list.map(|l| array(&l)).unwrap_or_default();
    let modes = formats.iter().map(|f| mode(api, f)).collect();
    (formats, modes)
}

/// One format as a mode; a format with no description has no size, which the selection skips.
fn mode(api: &Api, format: &AnyObject) -> Mode {
    let mut mode = Mode { width: 0, height: 0, rank: 0, rates: Vec::new() };
    if !responds(format, sel!(formatDescription)) {
        return mode;
    }
    // SAFETY: `formatDescription` returns a CMFormatDescriptionRef owned by the format.
    let desc: *const c_void = unsafe { send(format, sel!(formatDescription)) };
    if desc.is_null() {
        return mode;
    }
    // SAFETY: `desc` is a live video format description (the format retains it).
    let dims = unsafe { (api.format_dimensions)(desc) };
    (mode.width, mode.height) = (dims.width.max(0) as u32, dims.height.max(0) as u32);
    if responds(format, sel!(videoSupportedFrameRateRanges)) {
        // SAFETY: returns an NSArray of AVFrameRateRange.
        let ranges: Option<Obj> = unsafe { msg_send![format, videoSupportedFrameRateRanges] };
        for range in ranges.map(|r| array(&r)).unwrap_or_default() {
            // SAFETY: `minFrameDuration` and `maxFrameDuration` return CMTime.
            let (fastest, slowest): (CMTime, CMTime) =
                unsafe { (msg_send![&*range, minFrameDuration], msg_send![&*range, maxFrameDuration]) };
            if let (Some(fastest), Some(slowest)) = (fastest.interval(), slowest.interval()) {
                mode.rates.push(Rates { fastest, slowest });
            }
        }
    }
    mode
}

/// Switches `device` to the format nearest `spec` and fixes its frame duration there (both limits
/// to one value from that format's own ranges, as AVFoundation raises on any other). Returns
/// whether the device is left locked for configuration: the caller unlocks it after
/// `startRunning`, which would otherwise reset the format to the session preset's. A device that
/// can't be locked (another app holds it) keeps its current format; frames are scaled either way.
fn configure(api: &Api, device: &AnyObject, spec: FrameSpec) -> bool {
    let (formats, modes) = modes(api, device);
    let Some(choice) = select::nearest(&modes, spec) else { return false };
    let mut error: *mut NSError = ptr::null_mut();
    // SAFETY: `lockForConfiguration:` takes an NSError** out-parameter and returns BOOL.
    if !responds(device, sel!(lockForConfiguration:)) || !unsafe { msg_send![device, lockForConfiguration: &mut error] } {
        return false;
    }
    // SAFETY: the format is one of the device's own `formats`.
    let _: () = unsafe { msg_send![device, setActiveFormat: &*formats[choice.index]] };
    let duration = choice.interval.and_then(CMTime::of);
    if let Some(t) = duration
        && responds(device, sel!(setActiveVideoMinFrameDuration:))
        && responds(device, sel!(setActiveVideoMaxFrameDuration:))
    {
        // SAFETY: both take a CMTime; `t` is the request inside, or an end of, one of the active
        // format's frame rate ranges.
        unsafe {
            let _: () = msg_send![device, setActiveVideoMinFrameDuration: t];
            let _: () = msg_send![device, setActiveVideoMaxFrameDuration: t];
        }
    }
    true
}

/// `AVCaptureDeviceWasConnected` / `WasDisconnected` notifications as hot-plug signals. The
/// observers' blocks run on whatever thread posts the notification and only send on a channel.
pub(crate) struct DeviceNotifications {
    center: Obj,
    observers: Vec<Obj>,
    rx: Receiver<()>,
}

// SAFETY: NSNotificationCenter is thread-safe, and the observers are opaque tokens only passed
// back to it (`removeObserver:`); nothing else touches them.
unsafe impl Send for DeviceNotifications {}

/// `CMIOObjectPropertyAddress`.
#[repr(C)]
struct CmioAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

/// `OSStatus CMIOObjectSetPropertyData(CMIOObjectID, const CMIOObjectPropertyAddress *, UInt32
/// qualifierDataSize, const void *qualifierData, UInt32 dataSize, const void *data)`.
type CmioSetPropertyData = unsafe extern "C" fn(u32, *const CmioAddress, u32, *const c_void, u32, *const c_void) -> i32;

/// Makes CoreMediaIO post its device notifications from its own thread. AVFoundation's
/// `AVCaptureDeviceWasConnected` / `WasDisconnected` arrive on the run loop named by
/// `kCMIOHardwarePropertyRunLoop`, by default the main thread's, which a CLI or daemon never runs;
/// setting it to NULL moves delivery to CoreMediaIO's own thread. Once per process; the error says
/// why delivery is left as it was.
fn own_notification_thread() -> std::result::Result<(), String> {
    static DONE: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    DONE.get_or_init(|| {
        let lib = open(COREMEDIAIO).map_err(|e| e.to_string())?;
        let set = function::<CmioSetPropertyData>(lib, c"CMIOObjectSetPropertyData").map_err(|e| e.to_string())?;
        // 'rnlp' on the system object (1), global scope ('glob'), main element (0).
        let address = CmioAddress { selector: u32::from_be_bytes(*b"rnlp"), scope: u32::from_be_bytes(*b"glob"), element: 0 };
        let none: *const c_void = ptr::null();
        // SAFETY: the address and the pointer-sized data (a NULL CFRunLoopRef) outlive the call;
        // there is no qualifier.
        let status = unsafe { set(1, &address, 0, ptr::null(), size_of::<*const c_void>() as u32, (&raw const none).cast::<c_void>()) };
        if status == 0 { Ok(()) } else { Err(format!("CMIOObjectSetPropertyData failed with OSStatus {status}")) }
    })
    .clone()
}

pub(crate) fn watch() -> Result<Box<dyn Signals>> {
    let api = api()?;
    if let Err(note) = own_notification_thread() {
        // Notifications then depend on the host running a main run loop; the hub's stall
        // detection still reports an unplugged camera's capture.
        eprintln!("sim-camera: hot-plug notifications may not be delivered without a main run loop ({note})");
    }
    if api.plugged.len() < 2 {
        return Err(Error::Unavailable("AVCaptureDeviceWasConnectedNotification".into()));
    }
    autoreleasepool(|_| {
        // SAFETY: `+defaultCenter` returns the process' notification center.
        let center: Option<Obj> = unsafe { msg_send![class(c"NSNotificationCenter")?, defaultCenter] };
        let center = center.ok_or_else(|| Error::Unavailable("NSNotificationCenter".into()))?;
        let (tx, rx) = mpsc::channel();
        let mut watch = DeviceNotifications { center, observers: Vec::new(), rx };
        for &name in &api.plugged {
            let tx = tx.clone();
            let block = RcBlock::new(move |_note: *mut AnyObject| {
                let _ = tx.send(());
            });
            let none: *const AnyObject = ptr::null();
            // SAFETY: takes a notification name, a nil sender filter, a nil queue (run on the
            // posting thread) and a `void (^)(NSNotification *)` block, which it copies; returns
            // the observer token.
            let observer: Option<Obj> =
                unsafe { msg_send![&*watch.center, addObserverForName: object(name), object: none, queue: none, usingBlock: &*block] };
            watch.observers.extend(observer);
        }
        Ok(Box::new(watch) as Box<dyn Signals>)
    })
}

impl Signals for DeviceNotifications {
    fn wait(&mut self, timeout: Duration) -> Result<bool> {
        match self.rx.recv_timeout(timeout) {
            Ok(()) => {
                while self.rx.try_recv().is_ok() {}
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Ok(false),
        }
    }
}

impl Drop for DeviceNotifications {
    fn drop(&mut self) {
        for observer in &self.observers {
            // SAFETY: `removeObserver:` takes a token `addObserverForName:...` returned.
            let _: () = unsafe { msg_send![&*self.center, removeObserver: &**observer] };
        }
    }
}

/// A frame the delegate copied out, and when.
struct Arrived {
    width: u32,
    height: u32,
    bgra: Arc<[u8]>,
    at: Instant,
}

/// The newest frame from the camera.
#[derive(Default)]
struct Latest {
    frame: Mutex<Option<Arrived>>,
    arrived: Condvar,
}

struct Ivars {
    latest: Arc<Latest>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `SampleDelegate` doesn't implement
    // `Drop`; its ivars are `Send + Sync`.
    #[unsafe(super(NSObject))]
    #[name = "SimeeCameraSampleDelegate"]
    #[ivars = Ivars]
    struct SampleDelegate;

    impl SampleDelegate {
        /// `AVCaptureVideoDataOutputSampleBufferDelegate`: one BGRA frame, on the output's queue.
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: *mut AnyObject, sample: *mut c_void, _connection: *mut AnyObject) {
            // A panic must not unwind into Objective-C (it would abort the process): drop the frame.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Ok(api) = api() else { return };
                if let Some((width, height, px)) = copy_sample(api, sample, 0) {
                    let latest = &self.ivars().latest;
                    let arrived = Arrived { width, height, bgra: px.into(), at: Instant::now() };
                    *latest.frame.lock().unwrap_or_else(|p| p.into_inner()) = Some(arrived);
                    latest.arrived.notify_all();
                }
            }));
        }
    }

    // SAFETY: NSObjectProtocol asks nothing beyond NSObject's own implementation.
    unsafe impl NSObjectProtocol for SampleDelegate {}
);

impl SampleDelegate {
    fn new(latest: Arc<Latest>) -> Retained<SampleDelegate> {
        let this = SampleDelegate::alloc().set_ivars(Ivars { latest });
        // SAFETY: NSObject's `init` on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Copies a sample buffer's BGRA pixels into tight rows, turned `quarter_turns` clockwise.
fn copy_sample(api: &Api, sample: *mut c_void, quarter_turns: u8) -> Option<(u32, u32, Vec<u8>)> {
    if sample.is_null() {
        return None;
    }
    // SAFETY: `sample` is a live CMSampleBuffer: the one a delegate call was given, or one a
    // reader returned that the caller holds.
    let pb = unsafe { (api.sample_image)(sample) };
    // SAFETY: `pb` is the sample's image buffer (a CVPixelBuffer), valid during the call.
    if pb.is_null() || unsafe { (api.pixel_format)(pb) } != FORMAT_BGRA {
        return None;
    }
    // SAFETY: as above; read-only lock, released below.
    if unsafe { (api.lock)(pb, LOCK_READ_ONLY) } != 0 {
        return None;
    }
    // SAFETY: the buffer is locked, so its base address and geometry are valid until unlock.
    let (base, stride, w, h) = unsafe { ((api.base)(pb), (api.bytes_per_row)(pb), (api.width)(pb), (api.height)(pb)) };
    // A buffer reporting no pixels, or rows shorter than its width, is dropped, not sliced.
    let usable = !base.is_null()
        && w > 0
        && h > 0
        && w <= u32::MAX as usize
        && h <= u32::MAX as usize
        && stride > 0
        && stride <= u32::MAX as usize
        && w.checked_mul(4).is_some_and(|row| stride >= row);
    let out = usable
        .then(|| {
            // SAFETY: a locked BGRA buffer holds `h` rows of `stride` bytes at `base`.
            let src = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), stride * h) };
            crate::frame::turn(src, stride, w as u32, h as u32, quarter_turns)
        })
        .flatten();
    // SAFETY: balances the lock above.
    unsafe { (api.unlock)(pb, LOCK_READ_ONLY) };
    out
}

extern "C" fn drained(_: *mut c_void) {}

/// A host camera capturing through AVFoundation, at the camera's own format, scaled to each
/// request.
pub(crate) struct Webcam {
    name: String,
    device: Obj,
    session: Obj,
    output: Obj,
    _delegate: Retained<SampleDelegate>,
    queue: *mut c_void,
    latest: Arc<Latest>,
}

// SAFETY: AVCaptureSession, its outputs and devices may be messaged from any thread (AVFoundation
// recommends starting and stopping sessions off the main thread); one capture thread owns this
// and is the only one that messages them.
unsafe impl Send for Webcam {}

impl Webcam {
    pub(crate) fn open(camera: &HostCamera, spec: FrameSpec) -> Result<Webcam> {
        // Session setup autoreleases objects (and the NSError out-parameter): drain them here, not
        // when this (long-lived) thread exits.
        autoreleasepool(|_| Self::open_inner(camera, spec))
    }

    fn open_inner(camera: &HostCamera, spec: FrameSpec) -> Result<Webcam> {
        let api = api()?;
        let unique_id = NSString::from_str(&camera.id);
        // SAFETY: `+deviceWithUniqueID:` takes an NSString and returns a device or nil.
        let device: Option<Obj> = unsafe { msg_send![class(c"AVCaptureDevice")?, deviceWithUniqueID: &*unique_id] };
        let device = device.ok_or_else(|| Error::NotFound { query: camera.id.clone(), available: Vec::new() })?;
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: takes a device and an NSError** out-parameter; returns an input or nil.
        let input: Option<Obj> = unsafe { msg_send![class(c"AVCaptureDeviceInput")?, deviceInputWithDevice: &*device, error: &mut error] };
        let input = input.ok_or_else(|| {
            // SAFETY: on failure the out-parameter holds nil or an autoreleased NSError.
            let reason = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default();
            Error::Capture(format!("can't open {}: {reason}", camera.name))
        })?;
        // SAFETY: `+new` returns a new session.
        let session: Option<Obj> = unsafe { msg_send![class(c"AVCaptureSession")?, new] };
        let session = session.ok_or_else(|| Error::Unavailable("AVCaptureSession".into()))?;
        // SAFETY: `canAddInput:` takes an input and returns BOOL; `addInput:` is only sent when
        // it may be (otherwise it raises).
        if !unsafe { msg_send![&*session, canAddInput: &*input] } {
            return Err(Error::Capture(format!("{} is busy or can't be captured", camera.name)));
        }
        // SAFETY: checked above.
        let _: () = unsafe { msg_send![&*session, addInput: &*input] };

        // SAFETY: `+new` returns a new video data output.
        let output: Option<Obj> = unsafe { msg_send![class(c"AVCaptureVideoDataOutput")?, new] };
        let output = output.ok_or_else(|| Error::Unavailable("AVCaptureVideoDataOutput".into()))?;
        // SAFETY: `+numberWithUnsignedInt:` returns a number; `+dictionaryWithObject:forKey:`
        // takes a value and a copyable key (the CFString constant, toll-free bridged).
        let settings: Option<Obj> = unsafe {
            let number: Obj = msg_send![class(c"NSNumber")?, numberWithUnsignedInt: FORMAT_BGRA];
            msg_send![class(c"NSDictionary")?, dictionaryWithObject: &*number, forKey: object(api.pixel_format_key)]
        };
        let settings = settings.ok_or_else(|| Error::Unavailable("NSDictionary".into()))?;
        // SAFETY: `setVideoSettings:` takes a dictionary; `setAlwaysDiscardsLateVideoFrames:` a BOOL.
        unsafe {
            let _: () = msg_send![&*output, setVideoSettings: &*settings];
            let _: () = msg_send![&*output, setAlwaysDiscardsLateVideoFrames: true];
        }
        let latest = Arc::new(Latest::default());
        let delegate = SampleDelegate::new(latest.clone());
        // SAFETY: a NUL-terminated label and a null (serial) attribute.
        let queue = unsafe { dispatch_queue_create(c"simee.camera".as_ptr(), ptr::null()) };
        // SAFETY: takes the delegate and a dispatch queue (an object); the output retains the queue.
        let _: () = unsafe { msg_send![&*output, setSampleBufferDelegate: &*delegate, queue: queue.cast_const().cast::<AnyObject>()] };
        let webcam = Webcam { name: camera.name.clone(), device, session, output, _delegate: delegate, queue, latest };
        // SAFETY: `canAddOutput:` returns BOOL; `addOutput:` only when it may be.
        if !unsafe { msg_send![&*webcam.session, canAddOutput: &*webcam.output] } {
            return Err(Error::Capture(format!("{} can't deliver video frames", camera.name)));
        }
        // SAFETY: checked above.
        let _: () = unsafe { msg_send![&*webcam.session, addOutput: &*webcam.output] };
        let locked = configure(api, &webcam.device, spec);
        // SAFETY: `startRunning` blocks until the session runs or fails; `unlockForConfiguration`
        // balances `configure`'s lock.
        unsafe {
            let _: () = msg_send![&*webcam.session, startRunning];
            if locked {
                let _: () = msg_send![&*webcam.device, unlockForConfiguration];
            }
        }
        Ok(webcam)
    }

    /// Why frames aren't arriving: the permission, a disconnected camera, or neither.
    fn stalled(&self) -> Error {
        autoreleasepool(|_| self.stalled_inner())
    }

    fn stalled_inner(&self) -> Error {
        if let Ok(status) = authorization_status()
            && let Some(fix) = status.fix()
        {
            return Error::Permission { status, fix: fix.to_string() };
        }
        // SAFETY: `isConnected` returns BOOL.
        let connected: bool = unsafe { msg_send![&*self.device, isConnected] };
        if !connected {
            return Error::Missing(format!("{} was disconnected", self.name));
        }
        Error::Capture(format!("no frames from {} for {} s", self.name, FRAME_TIMEOUT.as_secs()))
    }
}

impl Source for Webcam {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        let mut guard = self.latest.frame.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(arrived) = guard.as_ref() {
                if arrived.at.elapsed() > FRAME_TIMEOUT {
                    drop(guard);
                    return Err(self.stalled());
                }
                let frame = CameraFrame { width: arrived.width, height: arrived.height, bgra: arrived.bgra.clone(), tick };
                drop(guard);
                return Ok(frame.fit(spec.width, spec.height));
            }
            let now = Instant::now();
            if now >= deadline {
                drop(guard);
                return Err(self.stalled());
            }
            guard = self.latest.arrived.wait_timeout(guard, deadline - now).map(|(g, _)| g).unwrap_or_else(|p| p.into_inner().0);
        }
    }
}

impl Drop for Webcam {
    fn drop(&mut self) {
        // SAFETY: `stopRunning` blocks until the session stops; a nil delegate and queue detach
        // the delegate; the empty synchronous block then waits out a callback already running on
        // the queue, after which the queue is released (the output holds its own reference).
        unsafe {
            let _: () = msg_send![&*self.session, stopRunning];
            let none: *const AnyObject = ptr::null();
            let _: () = msg_send![&*self.output, setSampleBufferDelegate: none, queue: none];
            dispatch_sync_f(self.queue, ptr::null_mut(), drained);
            dispatch_release(self.queue);
        }
    }
}

/// `AVAssetReaderStatusFailed`.
const READER_FAILED: isize = 3;

impl CMTime {
    /// The time as a duration from zero; `None` for an invalid time, zero for a negative one.
    fn duration(self) -> Option<Duration> {
        if self.flags & TIME_VALID == 0 || self.timescale <= 0 {
            return None;
        }
        let nanos = i128::from(self.value.max(0)) * 1_000_000_000 / i128::from(self.timescale);
        Some(Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)))
    }
}

/// `kCMTimeFlags_PositiveInfinity`.
const TIME_POSITIVE_INFINITY: u32 = 4;

impl CMTime {
    /// A duration from zero, in nanoseconds.
    fn at(d: Duration) -> CMTime {
        CMTime { value: i64::try_from(d.as_nanos()).unwrap_or(i64::MAX), timescale: 1_000_000_000, flags: TIME_VALID, epoch: 0 }
    }

    /// `kCMTimePositiveInfinity`.
    const FOREVER: CMTime = CMTime { value: 0, timescale: 0, flags: TIME_VALID | TIME_POSITIVE_INFINITY, epoch: 0 };
}

/// `CMTimeRange`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CMTimeRange {
    start: CMTime,
    duration: CMTime,
}

// SAFETY: the layout above is CoreMedia's `CMTimeRange` (`{?={?=qiIq}{?=qiIq}}`), an anonymous
// struct of two CMTimes.
unsafe impl Encode for CMTimeRange {
    const ENCODING: Encoding = Encoding::Struct("?", &[CMTime::ENCODING, CMTime::ENCODING]);
}

/// `CGSize`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Size {
    width: f64,
    height: f64,
}

// SAFETY: the layout above is CoreGraphics' `CGSize` on 64-bit targets (`{CGSize=dd}`).
unsafe impl Encode for Size {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

impl Transform {
    /// The quarter turns clockwise a track with this `preferredTransform` is shown at, to the
    /// nearest turn (a mirrored track's flip is not applied).
    fn quarter_turns(self) -> u8 {
        // A mirrored matrix (negative determinant) is a flip of x followed by the rotation; take
        // the rotation out of the unflipped column, so a horizontal mirror stays upright.
        let a = if self.a * self.d - self.b * self.c < 0.0 { -self.a } else { self.a };
        let turns = (self.b.atan2(a) / std::f64::consts::FRAC_PI_2).round();
        if turns.is_finite() { (turns as i64).rem_euclid(4) as u8 } else { 0 }
    }
}

/// Opens `video:<path>`: the file's first video track, decoded to BGRA and looped at its own rate.
pub(crate) fn open_video(path: &Path) -> Result<Box<dyn Source>> {
    let name = format!("video:{}", path.display());
    let reader = autoreleasepool(|_| VideoReader::open(&name, path))?;
    Ok(Box::new(crate::video::Player::new(name, reader)?))
}

/// A decoded frame as an `AVAssetReader` output it: a retained `CMSampleBufferRef`, released when
/// dropped. Its pixels are copied out only for a frame that is shown.
pub(crate) struct SampleBuffer(NonNull<c_void>);

// SAFETY: CoreMedia sample buffers are reference counted thread-safely and not changed once
// decoded; the capture thread that owns the player is the only one that reads this one.
unsafe impl Send for SampleBuffer {}

impl Drop for SampleBuffer {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            // SAFETY: balances the +1 reference `copyNextSampleBuffer` returned; dropped once.
            unsafe { (api.release)(self.0.as_ptr()) };
        }
    }
}

/// A video file's first video track through `AVAssetReader` with a BGRA track output, one reader
/// per pass of the file or jump within it (a reader can't rewind; a new one on the same asset,
/// started at a time, can).
pub(crate) struct VideoReader {
    name: String,
    track: Obj,
    asset: Obj,
    reader: Obj,
    output: Obj,
    interval: Duration,
    /// The track's rotation (`preferredTransform`), in quarter turns clockwise.
    turns: u8,
    /// Where the track starts and ends in the file, when it says.
    start: Duration,
    end: Option<Duration>,
}

// SAFETY: AVAsset, AVAssetTrack and AVAssetReader may be used from any thread, one at a time; the
// capture thread that owns the player is the only one that messages them.
unsafe impl Send for VideoReader {}

impl VideoReader {
    fn open(name: &str, path: &Path) -> Result<VideoReader> {
        std::fs::metadata(path).map_err(|e| Error::Io(format!("{name}: {e}")))?;
        let api = api()?;
        let url = file_url(path)?;
        let none: *const AnyObject = ptr::null();
        // SAFETY: `+URLAssetWithURL:options:` takes a file URL and nil options; returns an asset.
        let asset: Option<Obj> = unsafe { msg_send![class(c"AVURLAsset")?, URLAssetWithURL: &*url, options: none] };
        let asset = asset.ok_or_else(|| Error::Io(format!("{name}: AVFoundation can't open it")))?;
        if !responds(&asset, sel!(tracksWithMediaType:)) {
            return Err(Error::Unavailable("-[AVAsset tracksWithMediaType:]".into()));
        }
        // SAFETY: takes a media type and returns an NSArray of AVAssetTrack (loading the asset's
        // tracks synchronously, on this capture thread).
        let tracks: Option<Obj> = unsafe { msg_send![&*asset, tracksWithMediaType: object(api.video)] };
        let track = tracks
            .map(|t| array(&t))
            .unwrap_or_default()
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid(format!("{name} has no video track AVFoundation can read (is it a movie file?)")))?;
        // SAFETY: `nominalFrameRate` returns float, `naturalSize` CGSize, `preferredTransform`
        // CGAffineTransform and `timeRange` CMTimeRange: AVAssetTrack properties, read from the
        // track just loaded.
        let (fps, size, transform, range): (f32, Size, Transform, CMTimeRange) = unsafe {
            (
                msg_send![&*track, nominalFrameRate],
                msg_send![&*track, naturalSize],
                msg_send![&*track, preferredTransform],
                msg_send![&*track, timeRange],
            )
        };
        // Refused before a frame is decoded; each frame's own size is checked again as it shows.
        let side = |v: f64| if v.is_finite() && v >= 1.0 { v.ceil().min(f64::from(u32::MAX)) as u32 } else { 1 };
        crate::synth::check_size_of("video", side(size.width), side(size.height)).map_err(|e| Error::Invalid(format!("{name}: {e}")))?;
        let interval =
            if fps.is_finite() && fps >= 1.0 { Duration::from_secs_f64(1.0 / f64::from(fps)) } else { Duration::from_millis(33) };
        let start = range.start.duration().unwrap_or_default();
        let end = range.duration.duration().filter(|d| !d.is_zero()).map(|d| start + d);
        let (reader, output) = Self::start(api, name, &asset, &track, None)?;
        let turns = transform.quarter_turns();
        Ok(VideoReader { name: name.to_string(), track, asset, reader, output, interval, turns, start, end })
    }

    /// A reader started at file time `from` (the beginning of `track` when `None`), and its BGRA
    /// output.
    fn start(api: &Api, name: &str, asset: &AnyObject, track: &AnyObject, from: Option<Duration>) -> Result<(Obj, Obj)> {
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: takes an asset and an NSError** out-parameter; returns a reader or nil.
        let reader: Option<Obj> = unsafe { msg_send![class(c"AVAssetReader")?, assetReaderWithAsset: asset, error: &mut error] };
        let reader = reader.ok_or_else(|| Error::Io(format!("{name}: {}", describe_error(error))))?;
        if let Some(from) = from {
            if !responds(&reader, sel!(setTimeRange:)) {
                return Err(Error::Unavailable("-[AVAssetReader setTimeRange:]".into()));
            }
            let range = CMTimeRange { start: CMTime::at(from), duration: CMTime::FOREVER };
            // SAFETY: `setTimeRange:` takes a CMTimeRange, before `startReading` (below).
            let _: () = unsafe { msg_send![&*reader, setTimeRange: range] };
        }
        // SAFETY: `+numberWithUnsignedInt:` returns a number; `+dictionaryWithObject:forKey:`
        // takes a value and a copyable key (the CFString constant, toll-free bridged).
        let settings: Option<Obj> = unsafe {
            let number: Obj = msg_send![class(c"NSNumber")?, numberWithUnsignedInt: FORMAT_BGRA];
            msg_send![class(c"NSDictionary")?, dictionaryWithObject: &*number, forKey: object(api.pixel_format_key)]
        };
        let settings = settings.ok_or_else(|| Error::Unavailable("NSDictionary".into()))?;
        // SAFETY: takes a track of the reader's asset and decompression settings; returns an
        // output. `setAlwaysCopiesSampleData:` takes a BOOL (a shown frame is copied out anyway).
        let output: Option<Obj> =
            unsafe { msg_send![class(c"AVAssetReaderTrackOutput")?, assetReaderTrackOutputWithTrack: track, outputSettings: &*settings] };
        let output = output.ok_or_else(|| Error::Unavailable("AVAssetReaderTrackOutput".into()))?;
        // SAFETY: `canAddOutput:` returns BOOL; `addOutput:` only when it may be (otherwise it
        // raises); `startReading` returns BOOL.
        unsafe {
            let _: () = msg_send![&*output, setAlwaysCopiesSampleData: false];
            if !msg_send![&*reader, canAddOutput: &*output] {
                return Err(Error::Io(format!("{name}: its video track can't be decoded to BGRA")));
            }
            let _: () = msg_send![&*reader, addOutput: &*output];
            if !msg_send![&*reader, startReading] {
                let error: *mut NSError = msg_send![&*reader, error];
                return Err(Error::Io(format!("{name}: {}", describe_error(error))));
            }
        }
        Ok((reader, output))
    }

    fn next_inner(&mut self) -> Result<Option<Sample<SampleBuffer>>> {
        let api = api()?;
        loop {
            // SAFETY: `copyNextSampleBuffer` returns a retained CMSampleBufferRef, or NULL at the
            // end or on failure.
            let raw: *mut c_void = unsafe { send(&self.output, sel!(copyNextSampleBuffer)) };
            let Some(sample) = NonNull::new(raw).map(SampleBuffer) else {
                // SAFETY: `status` returns AVAssetReaderStatus (NSInteger); `error` an NSError or nil.
                let status: isize = unsafe { msg_send![&*self.reader, status] };
                if status == READER_FAILED {
                    // SAFETY: as above.
                    let error: *mut NSError = unsafe { msg_send![&*self.reader, error] };
                    return Err(Error::Capture(format!("{}: {}", self.name, describe_error(error))));
                }
                return Ok(None);
            };
            // SAFETY: `sample` is a live sample buffer, held until it is dropped; its image buffer
            // is its own (the Get rule: not released here), and a pixel buffer's format and size
            // may be read without locking it.
            let (pts, size) = unsafe {
                let pts = (api.presentation_time)(sample.0.as_ptr());
                let pb = (api.sample_image)(sample.0.as_ptr());
                let bgra = !pb.is_null() && (api.pixel_format)(pb) == FORMAT_BGRA;
                (pts, bgra.then(|| ((api.width)(pb), (api.height)(pb))))
            };
            // A sample with no image (or no valid time) carries nothing to show: skip it.
            let (Some(pts), Some((w, h))) = (pts.duration(), size) else { continue };
            let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else { continue };
            if w == 0 || h == 0 {
                continue;
            }
            let (width, height) = if self.turns % 2 == 1 { (h, w) } else { (w, h) };
            return Ok(Some(Sample { pts, width, height, data: sample }));
        }
    }
}

impl Decode for VideoReader {
    type Data = SampleBuffer;

    fn next(&mut self) -> Result<Option<Sample<SampleBuffer>>> {
        // The capture thread is long-lived: drain what each decode autoreleases.
        autoreleasepool(|_| self.next_inner())
    }

    fn pixels(&mut self, sample: &Sample<SampleBuffer>) -> Result<Vec<u8>> {
        let (_, _, bgra) = copy_sample(api()?, sample.data.0.as_ptr(), self.turns)
            .ok_or_else(|| Error::Capture(format!("{}: a decoded frame's pixels can't be read", self.name)))?;
        Ok(bgra)
    }

    /// A new reader from one frame before `to`, so the frame showing at `to` is among the first
    /// it gives whichever side of `to` the reader starts on.
    fn seek(&mut self, to: Duration) -> Result<()> {
        let from = Some(to.saturating_sub(self.interval)).filter(|&from| from > self.start);
        autoreleasepool(|_| {
            // SAFETY: `cancelReading` stops the current reader, in any state, before it is replaced.
            let _: () = unsafe { msg_send![&*self.reader, cancelReading] };
            (self.reader, self.output) = Self::start(api()?, &self.name, &self.asset, &self.track, from)?;
            Ok(())
        })
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn end(&self) -> Option<Duration> {
        self.end
    }
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        // SAFETY: `cancelReading` stops a reader in any state.
        let _: () = unsafe { msg_send![&*self.reader, cancelReading] };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourccs_are_big_endian() {
        assert_eq!(fourcc(u32::from_be_bytes(*b"420v")), "420v");
        assert_eq!(fourcc(crate::catalog::TRANSPORT_VIRTUAL), "virt");
        assert_eq!(fourcc(0), "????");
    }

    #[test]
    fn avfoundation_loads_and_discovers_device_types() {
        let api = api().unwrap_or_else(|e| panic!("{e}"));
        assert!(!api.device_types.is_empty(), "at least the built-in camera type");
        assert_eq!(api.plugged.len(), 2, "both hot-plug notifications");
    }

    /// CoreMediaIO accepts a NULL run loop (touches no camera: nothing is opened, no prompt). Live
    /// delivery of a real plug still needs the #335 live check.
    #[test]
    fn hot_plug_notifications_move_to_coremediaio_own_thread() {
        assert_eq!(own_notification_thread(), Ok(()));
    }

    /// Registers and removes the hot-plug observers (no camera is opened, nothing prompts).
    #[test]
    fn device_notifications_start_and_stop() {
        let mut signals = watch().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(signals.wait(Duration::from_millis(10)), Ok(false));
        drop(signals);
    }

    /// Reads each listed camera's formats and frame durations as modes, the way `configure` does
    /// before a capture, without opening, locking or capturing from it. Passes with no cameras.
    #[test]
    fn listed_cameras_formats_read_as_modes() {
        let api = api().unwrap_or_else(|e| panic!("{e}"));
        for camera in cameras().unwrap_or_default() {
            autoreleasepool(|_| {
                let unique_id = NSString::from_str(&camera.id);
                let device = class(c"AVCaptureDevice").unwrap_or_else(|e| panic!("{e}"));
                // SAFETY: `+deviceWithUniqueID:` takes an NSString and returns a device or nil.
                let device: Option<Obj> = unsafe { msg_send![device, deviceWithUniqueID: &*unique_id] };
                let Some(device) = device else { return };
                let (formats, modes) = modes(api, &device);
                assert_eq!(formats.len(), modes.len());
                let sized = modes.iter().filter(|m| m.width > 0).count();
                println!(
                    "{}: {} format(s), {sized} sized, choice for VGA {:?}",
                    camera.name,
                    modes.len(),
                    select::nearest(&modes, FrameSpec::default())
                );
                for r in modes.iter().flat_map(|m| &m.rates) {
                    assert!(r.fastest.fps() >= r.slowest.fps() - 1e-9, "{r:?}");
                }
            });
        }
    }

    #[test]
    fn cmtimes_convert_to_intervals_and_back_exactly() {
        let time = |value, timescale, flags| CMTime { value, timescale, flags, epoch: 0 };
        assert_eq!(time(1001, 30000, TIME_VALID).interval(), Some(Interval { num: 1001, den: 30000 }));
        assert_eq!(time(1, 30, 0).interval(), None, "not valid");
        assert_eq!(time(0, 30, TIME_VALID).interval(), None);
        assert_eq!(time(1, -30, TIME_VALID).interval(), None);
        let ntsc = Interval { num: 1001, den: 30000 };
        assert_eq!(CMTime::of(ntsc), Some(time(1001, 30000, TIME_VALID)));
        assert_eq!(CMTime::of(Interval { num: 1, den: u64::MAX }), None, "a timescale past i32");
        assert_eq!(CMTime::of(Interval { num: 0, den: 30 }), None);
        assert_eq!(size_of::<CMTime>(), 24);
    }

    /// `CVPixelBufferCreate`.
    type PixelBufferCreate = unsafe extern "C" fn(*const c_void, usize, usize, u32, *const c_void, *mut *mut c_void) -> i32;
    /// `-[AVAssetWriterInputPixelBufferAdaptor appendPixelBuffer:withPresentationTime:]`.
    type Append = unsafe extern "C" fn(*const AnyObject, Sel, *mut c_void, CMTime) -> Bool;

    fn ok<T>(r: Result<T>) -> T {
        r.unwrap_or_else(|e| panic!("{e}"))
    }

    fn dictionary(pairs: &[(&AnyObject, &AnyObject)]) -> Obj {
        // SAFETY: `+[NSMutableDictionary dictionary]` returns a new empty dictionary;
        // `setObject:forKey:` takes a non-nil value and a copyable key.
        unsafe {
            let dict: Obj = msg_send![ok(class(c"NSMutableDictionary")), dictionary];
            for (key, value) in pairs {
                let _: () = msg_send![&*dict, setObject: *value, forKey: *key];
            }
            dict
        }
    }

    fn number(n: u32) -> Obj {
        // SAFETY: `+numberWithUnsignedInt:` returns a number.
        unsafe { msg_send![ok(class(c"NSNumber")), numberWithUnsignedInt: n] }
    }

    /// Writes a QuickTime movie of solid BGRA frames, one per `colours` entry, at `fps`, with
    /// AVAssetWriter's H.264 encoder: a tiny test file generated in the test, so no fixture is
    /// checked in.
    pub(crate) fn write_clip(path: &std::path::Path, width: u32, height: u32, fps: i32, colours: &[[u8; 4]]) {
        write_movie(path, width, height, fps, IDENTITY, None, colours.len(), |i, _| colours[i]);
    }

    const IDENTITY: Transform = Transform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 };

    #[test]
    fn a_mirrored_track_keeps_its_rotation_and_never_turns_over() {
        let t = |a, b, c, d| Transform { a, b, c, d, tx: 0.0, ty: 0.0 }.quarter_turns();
        // Plain rotations.
        assert_eq!((t(1.0, 0.0, 0.0, 1.0), t(0.0, 1.0, -1.0, 0.0), t(-1.0, 0.0, 0.0, -1.0), t(0.0, -1.0, 1.0, 0.0)), (0, 1, 2, 3));
        // A horizontal mirror is upright (its flip is not applied), not upside down.
        assert_eq!(t(-1.0, 0.0, 0.0, 1.0), 0);
        // A mirror composed with each quarter turn keeps that turn.
        assert_eq!(t(0.0, 1.0, 1.0, 0.0), 1);
        assert_eq!(t(1.0, 0.0, 0.0, -1.0), 2);
        assert_eq!(t(0.0, -1.0, -1.0, 0.0), 3);
    }

    /// The track transform a recorder writes for a picture to be shown turned `turns` quarter
    /// turns clockwise (an iPhone held upright records `turns == 1`).
    fn clockwise(turns: u8, width: u32, height: u32) -> Transform {
        let (w, h) = (f64::from(width), f64::from(height));
        match turns % 4 {
            0 => IDENTITY,
            1 => Transform { a: 0.0, b: 1.0, c: -1.0, d: 0.0, tx: h, ty: 0.0 },
            2 => Transform { a: -1.0, b: 0.0, c: 0.0, d: -1.0, tx: w, ty: h },
            _ => Transform { a: 0.0, b: -1.0, c: 1.0, d: 0.0, tx: 0.0, ty: w },
        }
    }

    /// Writes `frames` frames at `fps`, frame `i`'s column `x` in colour `paint(i, x)`, with the
    /// track's `transform` (`preferredTransform` when read back) and at most `keyframes` frames
    /// between keyframes (the encoder's default when `None`).
    #[allow(clippy::too_many_arguments)]
    fn write_movie(
        path: &std::path::Path,
        width: u32,
        height: u32,
        fps: i32,
        transform: Transform,
        keyframes: Option<u32>,
        frames: usize,
        paint: impl Fn(usize, u32) -> [u8; 4],
    ) {
        autoreleasepool(|_| {
            let api = ok(api());
            let av = ok(open(AVFOUNDATION));
            let cv = ok(open(COREVIDEO));
            let named = |name: &CStr| object(constant(av, name).unwrap_or_else(|| panic!("{name:?}")));
            let create: PixelBufferCreate = ok(function(cv, c"CVPixelBufferCreate"));
            let _ = std::fs::remove_file(path);
            let url = ok(file_url(path));
            let mut error: *mut NSError = ptr::null_mut();
            // SAFETY: takes a file URL, a file type and an NSError** out-parameter; returns a
            // writer or nil.
            let writer: Option<Obj> = unsafe {
                msg_send![ok(class(c"AVAssetWriter")), assetWriterWithURL: &*url, fileType: named(c"AVFileTypeQuickTimeMovie"), error: &mut error]
            };
            let writer = writer.unwrap_or_else(|| panic!("AVAssetWriter: {}", describe_error(error)));
            let (w, h) = (number(width), number(height));
            let settings = dictionary(&[
                (named(c"AVVideoCodecKey"), named(c"AVVideoCodecTypeH264")),
                (named(c"AVVideoWidthKey"), &w),
                (named(c"AVVideoHeightKey"), &h),
            ]);
            if let Some(n) = keyframes {
                let n = number(n);
                let compression = dictionary(&[(named(c"AVVideoMaxKeyFrameIntervalKey"), &n)]);
                // SAFETY: `setObject:forKey:` on the mutable dictionary `dictionary` returned,
                // with a non-nil value and a copyable key.
                let _: () = unsafe { msg_send![&*settings, setObject: &*compression, forKey: named(c"AVVideoCompressionPropertiesKey")] };
            }
            let bgra = number(FORMAT_BGRA);
            let attributes = dictionary(&[(object(api.pixel_format_key), &bgra)]);
            // SAFETY: takes a media type and output settings and returns an input; the adaptor
            // takes the input and the source pixel buffer attributes.
            let (input, adaptor): (Obj, Obj) = unsafe {
                let input: Obj = msg_send![ok(class(c"AVAssetWriterInput")), assetWriterInputWithMediaType: object(api.video), outputSettings: &*settings];
                let _: () = msg_send![&*input, setExpectsMediaDataInRealTime: false];
                let _: () = msg_send![&*input, setTransform: transform];
                let adaptor: Obj = msg_send![ok(class(c"AVAssetWriterInputPixelBufferAdaptor")), assetWriterInputPixelBufferAdaptorWithAssetWriterInput: &*input, sourcePixelBufferAttributes: &*attributes];
                (input, adaptor)
            };
            // SAFETY: `addInput:` before `startWriting`; `startSessionAtSourceTime:` takes a CMTime.
            unsafe {
                let _: () = msg_send![&*writer, addInput: &*input];
                let started: bool = msg_send![&*writer, startWriting];
                assert!(started, "startWriting");
                let zero = CMTime { value: 0, timescale: fps, flags: TIME_VALID, epoch: 0 };
                let _: () = msg_send![&*writer, startSessionAtSourceTime: zero];
            }
            // SAFETY: `objc_msgSend` called through `appendPixelBuffer:withPresentationTime:`'s
            // signature (a CVPixelBufferRef and a CMTime, returning BOOL).
            let append: Append = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
            for i in 0..frames {
                let row_px: Vec<u8> = (0..width).flat_map(|x| paint(i, x)).collect();
                let deadline = Instant::now() + Duration::from_secs(10);
                // SAFETY: `isReadyForMoreMediaData` returns BOOL.
                while !unsafe { msg_send![&*input, isReadyForMoreMediaData] } {
                    assert!(Instant::now() < deadline, "the writer stopped taking frames");
                    std::thread::sleep(Duration::from_millis(5));
                }
                let mut pb: *mut c_void = ptr::null_mut();
                // SAFETY: a default allocator, the size, BGRA, no attributes and an out-pointer.
                let status = unsafe { create(ptr::null(), width as usize, height as usize, FORMAT_BGRA, ptr::null(), &mut pb) };
                assert!(status == 0 && !pb.is_null(), "CVPixelBufferCreate {status}");
                // SAFETY: the new buffer is locked for writing, filled row by row within its own
                // stride and height, then unlocked; `append` retains what it keeps, and the
                // reference `create` returned is released.
                unsafe {
                    assert_eq!((api.lock)(pb, 0), 0);
                    let (base, stride) = ((api.base)(pb).cast::<u8>(), (api.bytes_per_row)(pb));
                    let rows = std::slice::from_raw_parts_mut(base, stride * height as usize);
                    for row in rows.chunks_exact_mut(stride) {
                        row[..width as usize * 4].copy_from_slice(&row_px);
                    }
                    (api.unlock)(pb, 0);
                    let at = CMTime { value: i as i64, timescale: fps, flags: TIME_VALID, epoch: 0 };
                    let appended = append(&*adaptor, sel!(appendPixelBuffer:withPresentationTime:), pb, at);
                    (api.release)(pb);
                    assert!(appended.as_bool(), "frame {i} not appended");
                }
            }
            let (tx, rx) = mpsc::channel::<()>();
            let done = RcBlock::new(move || {
                let _ = tx.send(());
            });
            // SAFETY: `markAsFinished` ends the input; `finishWritingWithCompletionHandler:` takes
            // a `void (^)(void)` block, which it copies and calls once the file is written.
            unsafe {
                let _: () = msg_send![&*input, markAsFinished];
                let _: () = msg_send![&*writer, finishWritingWithCompletionHandler: &*done];
            }
            assert!(rx.recv_timeout(Duration::from_secs(20)).is_ok(), "the writer never finished");
            // SAFETY: `status` returns AVAssetWriterStatus (NSInteger); `error` an NSError or nil.
            let (status, error): (isize, *mut NSError) = unsafe { (msg_send![&*writer, status], msg_send![&*writer, error]) };
            assert_eq!(status, 2, "AVAssetWriterStatusCompleted: {}", describe_error(error));
        });
    }

    fn temp_clip(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sim-camera-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir.join(name)
    }

    fn near(px: [u8; 4], want: [u8; 4]) -> bool {
        px.iter().zip(want).all(|(&a, b)| a.abs_diff(b) <= 48)
    }

    const RED: [u8; 4] = [0, 0, 255, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    /// A generated 4-frame, 10 fps H.264 movie played as `video:`: each tick shows the frame the
    /// file shows at that time, looping every 400 ms, scaled to the request. Opens no camera.
    #[test]
    fn a_video_file_plays_at_its_own_rate_and_loops() {
        let path = temp_clip("bars.mov");
        write_clip(&path, 64, 48, 10, &[RED, GREEN, BLUE, WHITE]);
        let mut source = ok(crate::synth::open(&crate::SourceSpec::Video(path.clone())));
        assert!(!source.synthetic(), "frames come at the file's size, like a camera's");
        let spec = FrameSpec { width: 32, height: 24, fps: 30 };
        let mut shown = Vec::new();
        for ms in [0u64, 50, 100, 250, 350, 400, 520, 1_000_250] {
            let tick = Tick { seq: ms, pts: Duration::from_millis(ms), ..Tick::start() };
            let frame = source.frame(spec, tick).unwrap_or_else(|e| panic!("{ms} ms: {e}"));
            assert_eq!((frame.width, frame.height), (32, 24));
            shown.push((ms, frame.pixel(16, 12)));
        }
        let want = [RED, RED, GREEN, BLUE, WHITE, RED, GREEN, BLUE];
        for ((ms, px), want) in shown.iter().zip(want) {
            assert!(near(*px, want), "{ms} ms: {px:?}, want about {want:?}; all {shown:?}");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A landscape picture, red on the left and blue on the right, recorded with its track turned
    /// 0, 90, 180 and 270 degrees clockwise: each plays upright, at the turned size.
    #[test]
    fn a_rotated_recording_plays_upright() {
        let (w, h) = (64u32, 32u32);
        for turns in 0..4u8 {
            let path = temp_clip(&format!("turned-{turns}.mov"));
            write_movie(&path, w, h, 10, clockwise(turns, w, h), None, 2, |_, x| if x < w / 2 { RED } else { BLUE });
            let mut source = ok(crate::synth::open(&crate::SourceSpec::Video(path.clone())));
            let (sw, sh) = if turns % 2 == 1 { (h, w) } else { (w, h) };
            let frame = ok(source.frame(FrameSpec { width: sw, height: sh, fps: 10 }, Tick::start()));
            // The quarter of the turned picture each colour fills: (x, y) at its centre.
            let (red, blue) = match turns {
                0 => ((sw / 4, sh / 2), (sw * 3 / 4, sh / 2)),
                1 => ((sw / 2, sh / 4), (sw / 2, sh * 3 / 4)),
                2 => ((sw * 3 / 4, sh / 2), (sw / 4, sh / 2)),
                _ => ((sw / 2, sh * 3 / 4), (sw / 2, sh / 4)),
            };
            let (r, b) = (frame.pixel(red.0, red.1), frame.pixel(blue.0, blue.1));
            assert!(near(r, RED) && near(b, BLUE), "{turns} turn(s): {r:?} at {red:?}, {b:?} at {blue:?}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn a_missing_or_unreadable_video_file_says_why() {
        let missing = temp_clip("missing.mov");
        let err = crate::synth::open(&crate::SourceSpec::Video(missing)).err();
        assert!(matches!(&err, Some(Error::Io(m)) if m.contains("missing.mov")), "{err:?}");
        let text = temp_clip("notes.mov");
        std::fs::write(&text, b"not a movie").ok();
        let err = crate::synth::open(&crate::SourceSpec::Video(text.clone())).err();
        assert!(matches!(&err, Some(Error::Invalid(m) | Error::Io(m)) if m.contains("notes.mov")), "{err:?}");
        let _ = std::fs::remove_file(&text);
    }

    /// A 10 s, 30 fps H.264 clip with one keyframe, at its start: frame `i` is grey level
    /// `i * 255 / 299`, so a picture says roughly which frame it is. The key interval is only a
    /// maximum, and the encoder cuts a keyframe of its own now and then when the host is busy
    /// (seen with other encodes running), so a clip is written again until it has just the one.
    fn long_gop_clip(path: &std::path::Path) {
        // One at a time: the encoder cuts the most keyframes when several encodes run at once.
        static ENCODING: Mutex<()> = Mutex::new(());
        let _one = ENCODING.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..8 {
            write_movie(path, 64, 48, 30, IDENTITY, Some(3000), 300, |i, _| {
                let g = (i * 255 / 299) as u8;
                [g, g, g, 255]
            });
            let reader = ok(autoreleasepool(|_| VideoReader::open("video:long-gop", path)));
            if samples_and_keyframes(&reader.track) == (300, 1) {
                return;
            }
        }
        panic!("the encoder would not write a clip with a single keyframe");
    }

    /// `AVSampleCursorSyncInfo`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SyncInfo {
        full: Bool,
        partial: Bool,
        droppable: Bool,
    }

    // SAFETY: the three BOOL fields of `AVSampleCursorSyncInfo`, in order.
    unsafe impl Encode for SyncInfo {
        const ENCODING: Encoding = Encoding::Struct("?", &[Bool::ENCODING; 3]);
    }

    /// The samples of `track` and how many of them are keyframes (full sync samples), walking an
    /// `AVSampleCursor` through the file's samples in decode order.
    fn samples_and_keyframes(track: &AnyObject) -> (usize, usize) {
        // SAFETY: `makeSampleCursorAtFirstSampleInDecodeOrder` returns a cursor or nil;
        // `currentSampleSyncInfo` returns the sync struct and `stepInDecodeOrderByCount:`
        // the number of samples stepped (0 at the last), on that cursor.
        unsafe {
            let cursor: Option<Obj> = msg_send![track, makeSampleCursorAtFirstSampleInDecodeOrder];
            let cursor = cursor.expect("an AVSampleCursor for the track");
            let (mut samples, mut keyframes) = (0, 0);
            loop {
                let info: SyncInfo = msg_send![&*cursor, currentSampleSyncInfo];
                samples += 1;
                keyframes += usize::from(info.full.as_bool());
                let stepped: i64 = msg_send![&*cursor, stepInDecodeOrderByCount: 1i64];
                if stepped == 0 {
                    return (samples, keyframes);
                }
            }
        }
    }

    /// The long-GOP clip really is one: 300 samples, a single keyframe (so a seek into it lands
    /// in a GOP that starts at the clip's first frame, not in one the encoder cut short).
    /// `long_gop_clip` retries until it is, so this pins the helper and the keyframe count.
    #[test]
    fn the_long_gop_clip_has_a_single_keyframe() {
        let path = temp_clip("long-gop-keyframes.mov");
        long_gop_clip(&path);
        let reader = ok(autoreleasepool(|_| VideoReader::open("video:long-gop", &path)));
        assert_eq!(samples_and_keyframes(&reader.track), (300, 1));
        let _ = std::fs::remove_file(&path);
    }

    /// A reader started mid-GOP gives frames from the time it starts at, not from the keyframe
    /// before it (it decodes from there, but trims what it hands out).
    #[test]
    fn a_reader_started_mid_gop_gives_frames_from_its_start() {
        let path = temp_clip("long-gop-seek.mov");
        long_gop_clip(&path);
        let mut reader = ok(autoreleasepool(|_| VideoReader::open("video:long-gop", &path)));
        for to in [Duration::from_millis(5_000), Duration::from_millis(8_000)] {
            ok(reader.seek(to));
            let first = ok(reader.next()).map(|s| s.pts);
            let slack = reader.interval() + Duration::from_millis(1);
            assert!(first.is_some_and(|pts| pts <= to && to - pts <= slack), "a seek to {to:?} gave {first:?} first");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A `VideoReader` that counts its jumps; with `from_keyframe`, each jump starts at the clip's
    /// one keyframe, as a decoder that hands out a jump from its sync sample would.
    struct Jumps {
        reader: VideoReader,
        seeks: Arc<Mutex<usize>>,
        from_keyframe: bool,
    }

    impl Decode for Jumps {
        type Data = SampleBuffer;

        fn next(&mut self) -> Result<Option<Sample<SampleBuffer>>> {
            self.reader.next()
        }

        fn pixels(&mut self, sample: &Sample<SampleBuffer>) -> Result<Vec<u8>> {
            self.reader.pixels(sample)
        }

        fn seek(&mut self, to: Duration) -> Result<()> {
            *self.seeks.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            self.reader.seek(if self.from_keyframe { Duration::ZERO } else { to })
        }

        fn interval(&self) -> Duration {
            self.reader.interval()
        }

        fn end(&self) -> Option<Duration> {
            self.reader.end()
        }
    }

    /// Playback that starts 9 s into the long-GOP clip jumps once and plays on from there, also
    /// when the jump lands on the keyframe at 0, 270 frames behind (150 after one tick decodes).
    #[test]
    fn a_jump_into_a_long_gop_clip_catches_up_and_never_livelocks() {
        let path = temp_clip("long-gop-jump.mov");
        long_gop_clip(&path);
        for from_keyframe in [false, true] {
            let reader = ok(autoreleasepool(|_| VideoReader::open("video:long-gop", &path)));
            let seeks = Arc::new(Mutex::new(0));
            let jumps = Jumps { reader, seeks: seeks.clone(), from_keyframe };
            let mut player = ok(crate::video::Player::new("video:long-gop".into(), jumps));
            let spec = FrameSpec { width: 64, height: 48, fps: 30 };
            let mut shown = Vec::new();
            for ms in (9_000u64..=9_300).step_by(33) {
                let tick = Tick { seq: ms, pts: Duration::from_millis(ms), ..Tick::start() };
                shown.push(ok(player.frame(spec, tick)).pixel(32, 24)[0]);
            }
            // 9.297 s in: frame 278, grey 237.
            let last = shown.last().copied().unwrap_or_default();
            assert!(last.abs_diff(237) <= 16, "from_keyframe {from_keyframe}: grey {last}, want about 237; all {shown:?}");
            assert_eq!(*seeks.lock().unwrap_or_else(|e| e.into_inner()), 1, "from_keyframe {from_keyframe}: one jump; {shown:?}");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A Logitech C920's list as AVFoundation reports it (each format one range of 30 fps down to
    /// 5), chosen for requests the hub makes.
    #[test]
    fn webcam_requests_choose_the_nearest_avfoundation_format() {
        let range = |fps: i32| Rates {
            fastest: Interval { num: 1, den: fps as u64 },
            slowest: CMTime { value: 1, timescale: 5, flags: TIME_VALID, epoch: 0 }.interval().unwrap_or(Interval { num: 1, den: 5 }),
        };
        let formats = [(640, 480, 30), (800, 600, 24), (1280, 720, 30), (1600, 896, 15), (1920, 1080, 30), (2304, 1536, 2)];
        let modes: Vec<Mode> =
            formats.iter().map(|&(width, height, fps)| Mode { width, height, rank: 0, rates: vec![range(fps)] }).collect();
        let pick = |w, h, fps| select::nearest(&modes, FrameSpec { width: w, height: h, fps }).map(|c| (formats[c.index], c.interval));
        let per = |den| Some(Interval { num: 1, den });
        assert_eq!(pick(640, 480, 30), Some(((640, 480, 30), per(30))));
        assert_eq!(pick(1280, 720, 15), Some(((1280, 720, 30), per(15))));
        assert_eq!(pick(1920, 1080, 60), Some(((1920, 1080, 30), per(30))), "capped at the format's fastest");
        assert_eq!(pick(1600, 900, 30), Some(((1600, 896, 15), per(15))));
    }
}
