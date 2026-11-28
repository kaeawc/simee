//! VideoToolbox H.264: a real-time `VTCompressionSession` fed with `CVPixelBuffer`s that wrap a
//! device's framebuffer `IOSurface` (no copy) or come from an IOSurface-backed pool (one copy),
//! and a `VTDecompressionSession` that checks the output decodes.
//!
//! The FFI is thin: the session hands back each sample's bytes as VideoToolbox stores them
//! (length-prefixed NAL units, the format's NAL length size) and, on a keyframe, the format
//! description's parameter sets. Turning them into an Annex B stream is safe code in
//! `simee-encode`.
//!
//! Public frameworks, linked; the one constant newer than macOS 11
//! (`kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel`, macOS 12) is looked up at runtime.

use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use crate::display::Surface;
use crate::error::{Error, Result};
use crate::rt;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type OSStatus = i32;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CMTIME_VALID: u32 = 1;
/// Nanoseconds: what presentation times are stamped in, so a `Duration` comes back unchanged.
const TIMESCALE: i32 = 1_000_000_000;

impl CMTime {
    const INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };

    fn from_duration(d: Duration) -> CMTime {
        CMTime { value: i64::try_from(d.as_nanos()).unwrap_or(i64::MAX), timescale: TIMESCALE, flags: CMTIME_VALID, epoch: 0 }
    }

    fn to_duration(self) -> Option<Duration> {
        if self.flags & CMTIME_VALID == 0 || self.timescale <= 0 || self.value < 0 {
            return None;
        }
        let (v, s) = (self.value as u128, self.timescale as u128);
        Some(Duration::from_nanos(u64::try_from(v * 1_000_000_000 / s).unwrap_or(u64::MAX)))
    }
}

/// An opaque C struct only ever used by address.
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

type CompressionOutput = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void);
type DecompressionOutput = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void, CMTime, CMTime);

#[repr(C)]
struct DecompressionCallbackRecord {
    callback: DecompressionOutput,
    refcon: *mut c_void,
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(alloc: CFAllocatorRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFArrayCreate(alloc: CFAllocatorRef, values: *const CFTypeRef, count: isize, callbacks: *const Opaque) -> CFTypeRef;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    static kCFTypeArrayCallBacks: Opaque;
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMSampleBufferGetDataBuffer(sample: *mut c_void) -> *mut c_void;
    fn CMSampleBufferGetFormatDescription(sample: *mut c_void) -> *mut c_void;
    fn CMSampleBufferGetPresentationTimeStamp(sample: *mut c_void) -> CMTime;
    fn CMSampleBufferGetSampleAttachmentsArray(sample: *mut c_void, create: u8) -> CFTypeRef;
    fn CMBlockBufferGetDataLength(buffer: *mut c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(buffer: *mut c_void, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format: *mut c_void,
        index: usize,
        set: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        alloc: CFAllocatorRef,
        count: usize,
        sets: *const *const u8,
        sizes: *const usize,
        nal_header_len: i32,
        out: *mut *mut c_void,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        alloc: CFAllocatorRef,
        block: *mut c_void,
        block_len: usize,
        block_alloc: CFAllocatorRef,
        custom: *const c_void,
        offset: usize,
        len: usize,
        flags: u32,
        out: *mut *mut c_void,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(src: *const c_void, dest: *mut c_void, offset: usize, len: usize) -> OSStatus;
    fn CMSampleBufferCreateReady(
        alloc: CFAllocatorRef,
        data: *mut c_void,
        format: *mut c_void,
        samples: isize,
        timings: isize,
        timing: *const c_void,
        sizes: isize,
        size: *const usize,
        out: *mut *mut c_void,
    ) -> OSStatus;
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    fn CVPixelBufferCreateWithIOSurface(alloc: CFAllocatorRef, surface: *const c_void, attrs: CFTypeRef, out: *mut *mut c_void) -> i32;
    fn CVPixelBufferPoolCreate(alloc: CFAllocatorRef, pool_attrs: CFTypeRef, buffer_attrs: CFTypeRef, out: *mut *mut c_void) -> i32;
    fn CVPixelBufferPoolCreatePixelBuffer(alloc: CFAllocatorRef, pool: *mut c_void, out: *mut *mut c_void) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
}

#[allow(non_upper_case_globals)]
#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    fn VTCompressionSessionCreate(
        alloc: CFAllocatorRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFTypeRef,
        source_attrs: CFTypeRef,
        data_alloc: CFAllocatorRef,
        callback: CompressionOutput,
        refcon: *mut c_void,
        out: *mut *mut c_void,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: *mut c_void, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTSessionCopyProperty(session: *mut c_void, key: CFStringRef, alloc: CFAllocatorRef, out: *mut CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: *mut c_void) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: *mut c_void,
        image: *mut c_void,
        pts: CMTime,
        duration: CMTime,
        properties: CFTypeRef,
        frame_refcon: *mut c_void,
        info: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: *mut c_void, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: *mut c_void);
    fn VTDecompressionSessionCreate(
        alloc: CFAllocatorRef,
        format: *mut c_void,
        decoder_spec: CFTypeRef,
        dest_attrs: CFTypeRef,
        callback: *const DecompressionCallbackRecord,
        out: *mut *mut c_void,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: *mut c_void,
        sample: *mut c_void,
        flags: u32,
        refcon: *mut c_void,
        info: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: *mut c_void) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: *mut c_void);
}

const CODEC_H264: u32 = u32::from_be_bytes(*b"avc1");
const NUMBER_SINT32: isize = 3;
const NUMBER_SINT64: isize = 4;
const NUMBER_FLOAT64: isize = 6;
/// `kVTEncodeInfo_FrameDropped`.
const INFO_FRAME_DROPPED: u32 = 1 << 1;
/// `kVTCouldNotFindVideoEncoderErr`: no encoder matches the specification (no hardware encoder
/// when one is required).
const COULD_NOT_FIND_ENCODER: OSStatus = -12908;

/// An owned Core Foundation reference, released on drop.
struct Cf(CFTypeRef);

impl Cf {
    fn new(op: &'static str, r: CFTypeRef) -> Result<Cf> {
        if r.is_null() { Err(Error::Service { op, message: "Core Foundation returned NULL".into() }) } else { Ok(Cf(r)) }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null reference this value owns (created or copied).
        unsafe { CFRelease(self.0) }
    }
}

fn number_i32(v: i32) -> Result<Cf> {
    // SAFETY: `v` is a live i32 for the call, the kind says so; CFNumberCreate copies it.
    Cf::new("CFNumberCreate", unsafe { CFNumberCreate(ptr::null(), NUMBER_SINT32, (&v as *const i32).cast()) })
}

fn number_i64(v: i64) -> Result<Cf> {
    // SAFETY: as in `number_i32`, for an i64.
    Cf::new("CFNumberCreate", unsafe { CFNumberCreate(ptr::null(), NUMBER_SINT64, (&v as *const i64).cast()) })
}

fn number_f64(v: f64) -> Result<Cf> {
    // SAFETY: as in `number_i32`, for an f64.
    Cf::new("CFNumberCreate", unsafe { CFNumberCreate(ptr::null(), NUMBER_FLOAT64, (&v as *const f64).cast()) })
}

fn boolean(v: bool) -> CFTypeRef {
    // SAFETY: the boolean singletons are immutable constants that live for the process.
    unsafe { if v { kCFBooleanTrue } else { kCFBooleanFalse } }
}

/// A CFDictionary of CF keys and values (both retained by the dictionary).
fn dictionary(pairs: &[(CFStringRef, CFTypeRef)]) -> Result<Cf> {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: both arrays hold `pairs.len()` live CF objects; the CFType callbacks retain them.
    let d = unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    };
    Cf::new("CFDictionaryCreate", d)
}

fn array(values: &[CFTypeRef]) -> Result<Cf> {
    // SAFETY: `values` holds live CF objects; the CFType callbacks retain them.
    Cf::new("CFArrayCreate", unsafe { CFArrayCreate(ptr::null(), values.as_ptr(), values.len() as isize, &kCFTypeArrayCallBacks) })
}

fn check(op: &'static str, status: OSStatus) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(Error::Service { op, message: format!("OSStatus {status}") }) }
}

/// The byte order of the frames a [`PixelBufferPool`] is filled with: an iOS framebuffer is BGRA;
/// emu's frames are RGBA. The pool's buffers are BGRA either way: on this host CoreVideo allocates
/// no IOSurface-backed `'RGBA'` or `'ARGB'` buffers (`kCVReturnInvalidPixelFormat`, -6680), so RGBA
/// rows have red and blue swapped in the one pass that copies them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Rgba,
}

const FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// A retained `CVPixelBuffer`.
pub struct PixelBuffer(*mut c_void);

// SAFETY: a CVPixelBuffer is a reference-counted CF object whose retain and release are
// thread-safe; this handle owns one reference and only reads its geometry.
unsafe impl Send for PixelBuffer {}

impl PixelBuffer {
    /// Wraps a framebuffer surface without copying it: the encoder reads the surface itself
    /// (converting BGRA to YUV on the GPU or media engine). The device keeps rendering into it,
    /// so a frame can catch the middle of an update, as a screen recording does.
    pub fn wrap_surface(surface: &Surface) -> Result<PixelBuffer> {
        let mut out = ptr::null_mut();
        // SAFETY: `surface` is a live, retained IOSurface; null attributes use the surface's own
        // format and geometry; `out` is written on success, and the buffer retains the surface.
        let rc = unsafe { CVPixelBufferCreateWithIOSurface(ptr::null(), surface.as_raw(), ptr::null(), &mut out) };
        if rc != 0 || out.is_null() {
            return Err(Error::Service { op: "CVPixelBufferCreateWithIOSurface", message: format!("CVReturn {rc}") });
        }
        Ok(PixelBuffer(out))
    }

    pub fn size(&self) -> (u32, u32) {
        // SAFETY: `self.0` is a live pixel buffer.
        unsafe { (CVPixelBufferGetWidth(self.0) as u32, CVPixelBufferGetHeight(self.0) as u32) }
    }

    /// The CoreVideo pixel format, as its four characters (`BGRA`, `ARGB`).
    pub fn fourcc(&self) -> [u8; 4] {
        // SAFETY: `self.0` is a live pixel buffer.
        unsafe { CVPixelBufferGetPixelFormatType(self.0) }.to_be_bytes()
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        // SAFETY: this handle owns one reference to a live pixel buffer.
        unsafe { CFRelease(self.0.cast_const()) }
    }
}

/// An IOSurface-backed `CVPixelBufferPool` of one size and format, for frames that arrive as bytes
/// (Android's): [`PixelBufferPool::fill`] copies a frame in, the one copy on its way to the encoder.
pub struct PixelBufferPool {
    pool: *mut c_void,
    width: u32,
    height: u32,
    format: PixelFormat,
}

// SAFETY: a CVPixelBufferPool is a thread-safe CF object; this handle owns one reference.
unsafe impl Send for PixelBufferPool {}

impl PixelBufferPool {
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Result<PixelBufferPool> {
        let (w, h) = (i32::try_from(width), i32::try_from(height));
        let (Ok(w), Ok(h)) = (w, h) else { return Err(Error::Invalid(format!("pool size {width}x{height}"))) };
        if w <= 0 || h <= 0 {
            return Err(Error::Invalid(format!("pool size {width}x{height}")));
        }
        let (fmt, w, h, iosurface) = (number_i64(i64::from(FORMAT_BGRA))?, number_i32(w)?, number_i32(h)?, dictionary(&[])?);
        // SAFETY: CoreVideo's constant strings, live for the process.
        let keys = unsafe {
            [kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey]
        };
        let attrs = dictionary(&[(keys[0], fmt.0), (keys[1], w.0), (keys[2], h.0), (keys[3], iosurface.0)])?;
        let mut pool = ptr::null_mut();
        // SAFETY: `attrs` is a live dictionary; `pool` is written on success.
        let rc = unsafe { CVPixelBufferPoolCreate(ptr::null(), ptr::null(), attrs.0, &mut pool) };
        if rc != 0 || pool.is_null() {
            return Err(Error::Service { op: "CVPixelBufferPoolCreate", message: format!("CVReturn {rc}") });
        }
        Ok(PixelBufferPool { pool, width, height, format })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// A buffer from the pool holding `pixels`: `height` rows of `stride` bytes, each starting with
    /// `width * 4` bytes of pixels in the pool's [`PixelFormat`]. One copy: a single `memcpy` when
    /// the strides match.
    pub fn fill(&self, pixels: &[u8], stride: usize) -> Result<PixelBuffer> {
        let row = self.width as usize * 4;
        let rows = self.height as usize;
        if stride < row || pixels.len() < stride * (rows - 1) + row {
            return Err(Error::Invalid(format!("{} bytes for {}x{} at stride {stride}", pixels.len(), self.width, self.height)));
        }
        let mut out = ptr::null_mut();
        // SAFETY: `self.pool` is live; `out` is written on success.
        let rc = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), self.pool, &mut out) };
        if rc != 0 || out.is_null() {
            return Err(Error::Service { op: "CVPixelBufferPoolCreatePixelBuffer", message: format!("CVReturn {rc}") });
        }
        let buffer = PixelBuffer(out);
        // SAFETY: `out` is a live pixel buffer; flags 0 locks for writing.
        check("CVPixelBufferLockBaseAddress", unsafe { CVPixelBufferLockBaseAddress(out, 0) })?;
        // SAFETY: the buffer is locked, so its base address and stride are stable.
        let (base, dst_stride) = unsafe { (CVPixelBufferGetBaseAddress(out).cast::<u8>(), CVPixelBufferGetBytesPerRow(out)) };
        let copied = !base.is_null() && dst_stride >= row && {
            // SAFETY: while locked, the buffer's `rows` rows of `dst_stride` bytes are ours to
            // write; the slice ends before the lock is released below.
            let dst = unsafe { std::slice::from_raw_parts_mut(base, dst_stride * (rows - 1) + row) };
            match self.format {
                PixelFormat::Rgba => sim_core::pixels::swap_red_blue_into(pixels, stride, dst, dst_stride, self.width as usize, rows),
                PixelFormat::Bgra if dst_stride == stride => {
                    // One copy of the whole frame.
                    dst.copy_from_slice(&pixels[..dst.len()]);
                    true
                }
                PixelFormat::Bgra => {
                    for (y, d) in dst.chunks_mut(dst_stride).enumerate() {
                        d[..row].copy_from_slice(&pixels[y * stride..y * stride + row]);
                    }
                    true
                }
            }
        };
        // SAFETY: balances the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(out, 0) };
        if !copied {
            return Err(Error::Service { op: "CVPixelBufferGetBaseAddress", message: "the pool's buffer has no usable memory".into() });
        }
        Ok(buffer)
    }
}

impl Drop for PixelBufferPool {
    fn drop(&mut self) {
        // SAFETY: this handle owns one reference to a live pool.
        unsafe { CFRelease(self.pool.cast_const()) }
    }
}

/// The H.264 profile to ask for; the level follows the frame size and rate (`AutoLevel`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H264Profile {
    /// Baseline with `constraint_set1`: what every WebRTC endpoint decodes (macOS 12 on).
    ConstrainedBaseline,
    Baseline,
    Main,
    High,
}

/// Whether the session runs on the media engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hardware {
    /// Fail when no hardware encoder takes the session.
    Require,
    /// Use one when there is one, else the software encoder ([`CompressionSession::hardware`]
    /// says which).
    Prefer,
    /// The software encoder.
    Avoid,
}

/// What a session is created with. Frames of another size are scaled to this one by VideoToolbox.
#[derive(Clone, Debug)]
pub struct H264Config {
    pub width: u32,
    pub height: u32,
    pub profile: H264Profile,
    pub hardware: Hardware,
    /// `kVTCompressionPropertyKey_RealTime`.
    pub real_time: bool,
    /// The most frames between keyframes; 0 leaves it to the encoder.
    pub max_keyframe_interval: u32,
    /// The longest time between keyframes, if any.
    pub max_keyframe_interval_duration: Option<Duration>,
    /// Target bits per second.
    pub average_bitrate: u32,
    /// Hard limits: at most `bytes` in any window of `seconds` (`DataRateLimits`), each pair one limit.
    pub data_rate_limits: Vec<(u64, f64)>,
    /// The rate frames are expected at.
    pub expected_fps: f64,
}

/// One compressed picture as VideoToolbox emitted it.
#[derive(Clone, Debug, Default)]
pub struct EncodedSample {
    /// The presentation time it was submitted with.
    pub pts: Duration,
    pub keyframe: bool,
    /// The encoder dropped the frame: no data.
    pub dropped: bool,
    /// NAL units, each behind a big-endian length of `nal_length_size` bytes.
    pub data: Vec<u8>,
    pub nal_length_size: u8,
    /// On a keyframe: the format description's parameter sets (SPS, then PPS), header byte first.
    pub parameter_sets: Vec<Vec<u8>>,
}

/// What the output callback collects.
struct Outputs {
    samples: Mutex<Vec<std::result::Result<EncodedSample, OSStatus>>>,
}

extern "C" fn on_compressed(refcon: *mut c_void, _frame: *mut c_void, status: OSStatus, info: u32, sample: *mut c_void) {
    // SAFETY: `refcon` is the session's `Outputs`, which outlives the session (dropped after it
    // is invalidated, which waits for outstanding callbacks).
    let outputs = unsafe { &*refcon.cast::<Outputs>() };
    let item = if status != 0 {
        Err(status)
    } else if sample.is_null() || info & INFO_FRAME_DROPPED != 0 {
        Ok(EncodedSample { dropped: true, ..EncodedSample::default() })
    } else {
        // SAFETY: a non-null sample buffer is valid for the duration of the callback.
        unsafe { read_sample(sample) }
    };
    if let Ok(mut samples) = outputs.samples.lock() {
        samples.push(item);
    }
}

/// Copies one compressed sample's bytes, time, sync flag and (on a keyframe) parameter sets.
///
/// # Safety
/// `sample` must be a valid `CMSampleBufferRef` for the duration of the call.
unsafe fn read_sample(sample: *mut c_void) -> std::result::Result<EncodedSample, OSStatus> {
    // SAFETY: the caller passes a live sample buffer; every getter below borrows from it.
    unsafe {
        let pts = CMSampleBufferGetPresentationTimeStamp(sample).to_duration().unwrap_or_default();
        let block = CMSampleBufferGetDataBuffer(sample);
        let mut data = Vec::new();
        if !block.is_null() {
            let len = CMBlockBufferGetDataLength(block);
            data.resize(len, 0);
            let rc = CMBlockBufferCopyDataBytes(block, 0, len, data.as_mut_ptr().cast());
            if rc != 0 {
                return Err(rc);
            }
        }
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        let not_sync = if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let first = CFArrayGetValueAtIndex(attachments, 0);
            let v = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
            !v.is_null() && CFBooleanGetValue(v) != 0
        } else {
            false
        };
        let keyframe = !not_sync;
        let format = CMSampleBufferGetFormatDescription(sample);
        let mut nal_length_size = 4;
        let mut parameter_sets = Vec::new();
        if !format.is_null() {
            let (mut set, mut size, mut count, mut header) = (ptr::null(), 0usize, 0usize, 0i32);
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, &mut set, &mut size, &mut count, &mut header) == 0 {
                nal_length_size = header.clamp(1, 4) as u8;
                if keyframe {
                    for i in 0..count {
                        let (mut set, mut size) = (ptr::null(), 0usize);
                        let rc = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                            format,
                            i,
                            &mut set,
                            &mut size,
                            ptr::null_mut(),
                            ptr::null_mut(),
                        );
                        if rc == 0 && !set.is_null() {
                            // The set is owned by the format description, live for this call.
                            parameter_sets.push(std::slice::from_raw_parts(set, size).to_vec());
                        }
                    }
                }
            }
        }
        Ok(EncodedSample { pts, keyframe, dropped: false, data, nal_length_size, parameter_sets })
    }
}

/// A `VTCompressionSession` producing H.264.
pub struct CompressionSession {
    session: *mut c_void,
    outputs: Box<Outputs>,
    force_keyframe: Cf,
    hardware: bool,
}

// SAFETY: a VTCompressionSession may be used from any thread (one at a time, which `&mut self`
// ensures); the outputs are behind a mutex.
unsafe impl Send for CompressionSession {}

impl CompressionSession {
    pub fn new(config: &H264Config) -> Result<CompressionSession> {
        let (Ok(width), Ok(height)) = (i32::try_from(config.width), i32::try_from(config.height)) else {
            return Err(Error::Invalid(format!("frame size {}x{}", config.width, config.height)));
        };
        if width < 16 || height < 16 {
            return Err(Error::Invalid(format!("frame size {width}x{height} is below 16x16")));
        }
        let profile = profile_level(config.profile)?;
        // SAFETY: the specification keys are VideoToolbox's constant strings.
        let (enable, require) = unsafe {
            (
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
            )
        };
        let spec = dictionary(&[match config.hardware {
            Hardware::Require => (require, boolean(true)),
            Hardware::Prefer => (enable, boolean(true)),
            Hardware::Avoid => (enable, boolean(false)),
        }])?;
        // Built before the session exists, so a failure here has nothing to release.
        // SAFETY: VideoToolbox's constant string.
        let key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let force_keyframe = dictionary(&[(key, boolean(true))])?;
        let outputs = Box::new(Outputs { samples: Mutex::new(Vec::new()) });
        let refcon = (&*outputs as *const Outputs).cast_mut().cast::<c_void>();
        let mut session = ptr::null_mut();
        // SAFETY: `spec` is a live dictionary; null source attributes and allocators are allowed;
        // the callback matches VTCompressionOutputCallback and `refcon` outlives the session.
        let rc = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                width,
                height,
                CODEC_H264,
                spec.0,
                ptr::null(),
                ptr::null(),
                on_compressed,
                refcon,
                &mut session,
            )
        };
        if rc == COULD_NOT_FIND_ENCODER {
            return Err(Error::Unavailable(format!("no H.264 encoder for {width}x{height} with hardware {:?}", config.hardware)));
        }
        check("VTCompressionSessionCreate", rc)?;
        if session.is_null() {
            return Err(Error::Service { op: "VTCompressionSessionCreate", message: "no session".into() });
        }
        let mut s = CompressionSession { session, outputs, force_keyframe, hardware: false };
        s.configure(config, profile)?;
        // SAFETY: the session is live.
        check("VTCompressionSessionPrepareToEncodeFrames", unsafe { VTCompressionSessionPrepareToEncodeFrames(s.session) })?;
        s.hardware = s.query_hardware();
        Ok(s)
    }

    fn set(&self, op: &'static str, key: CFStringRef, value: CFTypeRef) -> Result<()> {
        // SAFETY: the session is live; `key` is a VideoToolbox constant and `value` a live CF object.
        check(op, unsafe { VTSessionSetProperty(self.session, key, value) })
    }

    fn configure(&self, config: &H264Config, profile: CFStringRef) -> Result<()> {
        // SAFETY: every key below is one of VideoToolbox's constant strings, live for the process.
        unsafe {
            self.set("RealTime", kVTCompressionPropertyKey_RealTime, boolean(config.real_time))?;
            self.set("ProfileLevel", kVTCompressionPropertyKey_ProfileLevel, profile)?;
            self.set("AllowFrameReordering", kVTCompressionPropertyKey_AllowFrameReordering, boolean(false))?;
            if config.max_keyframe_interval > 0 {
                let n = number_i32(i32::try_from(config.max_keyframe_interval).unwrap_or(i32::MAX))?;
                self.set("MaxKeyFrameInterval", kVTCompressionPropertyKey_MaxKeyFrameInterval, n.0)?;
            }
            if let Some(d) = config.max_keyframe_interval_duration {
                let n = number_f64(d.as_secs_f64())?;
                self.set("MaxKeyFrameIntervalDuration", kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, n.0)?;
            }
            if config.expected_fps > 0.0 {
                let n = number_f64(config.expected_fps)?;
                self.set("ExpectedFrameRate", kVTCompressionPropertyKey_ExpectedFrameRate, n.0)?;
            }
        }
        self.set_bitrate(config.average_bitrate, &config.data_rate_limits)
    }

    /// Retargets the encoder: an average bitrate and hard data-rate limits (`(bytes, seconds)`
    /// pairs; none leaves the current ones).
    pub fn set_bitrate(&self, average_bps: u32, limits: &[(u64, f64)]) -> Result<()> {
        let n = number_i32(i32::try_from(average_bps).unwrap_or(i32::MAX))?;
        // SAFETY: VideoToolbox's constant strings.
        let (average, limit) = unsafe { (kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_DataRateLimits) };
        self.set("AverageBitRate", average, n.0)?;
        if limits.is_empty() {
            return Ok(());
        }
        let mut owned = Vec::with_capacity(limits.len() * 2);
        for &(bytes, seconds) in limits {
            owned.push(number_i64(i64::try_from(bytes).unwrap_or(i64::MAX))?);
            owned.push(number_f64(seconds)?);
        }
        let refs: Vec<CFTypeRef> = owned.iter().map(|c| c.0).collect();
        let list = array(&refs)?;
        self.set("DataRateLimits", limit, list.0)
    }

    fn query_hardware(&self) -> bool {
        let mut value: CFTypeRef = ptr::null();
        // SAFETY: the session is live; a copied value is ours to release.
        let rc = unsafe {
            VTSessionCopyProperty(self.session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, ptr::null(), &mut value)
        };
        if rc != 0 || value.is_null() {
            return false;
        }
        let value = Cf(value);
        // SAFETY: the property's value is a CFBoolean.
        unsafe { CFBooleanGetValue(value.0) != 0 }
    }

    /// Whether the session runs on the hardware encoder (`UsingHardwareAcceleratedVideoEncoder`).
    pub fn hardware(&self) -> bool {
        self.hardware
    }

    /// Submits one picture with its presentation time; `keyframe` forces an IDR
    /// (`kVTEncodeFrameOptionKey_ForceKeyFrame`). Its output arrives with [`Self::complete`].
    pub fn encode(&mut self, image: &PixelBuffer, pts: Duration, keyframe: bool) -> Result<()> {
        let props = if keyframe { self.force_keyframe.0 } else { ptr::null() };
        let mut info = 0u32;
        // SAFETY: the session and pixel buffer are live; the properties are null or a live
        // dictionary; an invalid duration is allowed.
        let rc = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                image.0,
                CMTime::from_duration(pts),
                CMTime::INVALID,
                props,
                ptr::null_mut(),
                &mut info,
            )
        };
        check("VTCompressionSessionEncodeFrame", rc)
    }

    /// Waits until every picture submitted up to `until` (all of them for `None`) has come out,
    /// and returns what came out since the last call, in order.
    pub fn complete(&mut self, until: Option<Duration>) -> Result<Vec<EncodedSample>> {
        let until = until.map_or(CMTime::INVALID, CMTime::from_duration);
        // SAFETY: the session is live.
        check("VTCompressionSessionCompleteFrames", unsafe { VTCompressionSessionCompleteFrames(self.session, until) })?;
        self.take()
    }

    /// What has come out so far, without waiting.
    pub fn take(&mut self) -> Result<Vec<EncodedSample>> {
        let drained = std::mem::take(&mut *self.outputs.samples.lock().map_err(|_| Error::Invalid("poisoned".into()))?);
        drained
            .into_iter()
            .map(|s| s.map_err(|rc| Error::Service { op: "VTCompressionOutputCallback", message: format!("OSStatus {rc}") }))
            .collect()
    }
}

impl Drop for CompressionSession {
    fn drop(&mut self) {
        // SAFETY: the session is live; completing then invalidating it stops every callback
        // before `outputs` is freed, and the session's one reference is released last.
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, CMTime::INVALID);
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast_const());
        }
    }
}

fn profile_level(profile: H264Profile) -> Result<CFStringRef> {
    if profile == H264Profile::ConstrainedBaseline {
        let sym = rt::default_symbol(c"kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel")
            .map_err(|_| Error::Unavailable("Constrained Baseline needs macOS 12 or later".into()))?;
        // SAFETY: the symbol is a `const CFStringRef` VideoToolbox exports, live for the process.
        return Ok(unsafe { *sym.as_ptr().cast::<CFStringRef>() });
    }
    // SAFETY: VideoToolbox's constant strings, live for the process.
    Ok(unsafe {
        match profile {
            H264Profile::Main => kVTProfileLevel_H264_Main_AutoLevel,
            H264Profile::High => kVTProfileLevel_H264_High_AutoLevel,
            _ => kVTProfileLevel_H264_Baseline_AutoLevel,
        }
    })
}

/// What the decode callback collects: each picture's size, or the status that failed it.
struct Decoded {
    pictures: Mutex<Vec<std::result::Result<(u32, u32), OSStatus>>>,
}

extern "C" fn on_decoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: OSStatus,
    _info: u32,
    image: *mut c_void,
    _pts: CMTime,
    _d: CMTime,
) {
    // SAFETY: `refcon` is the decoder's `Decoded`, which outlives the session.
    let decoded = unsafe { &*refcon.cast::<Decoded>() };
    let item = if status != 0 {
        Err(status)
    } else if image.is_null() {
        Err(-1)
    } else {
        // SAFETY: a non-null image buffer is valid for the callback; a decoder's are pixel buffers.
        Ok(unsafe { (CVPixelBufferGetWidth(image) as u32, CVPixelBufferGetHeight(image) as u32) })
    };
    if let Ok(mut p) = decoded.pictures.lock() {
        p.push(item);
    }
}

/// A `VTDecompressionSession` for checking an encoder's output: each sample decodes to a picture
/// of the expected size.
pub struct H264Decoder {
    session: *mut c_void,
    format: Cf,
    decoded: Box<Decoded>,
}

impl H264Decoder {
    /// From the stream's SPS and PPS (header byte first); samples carry `nal_length_size`-byte
    /// lengths.
    pub fn new(parameter_sets: &[&[u8]], nal_length_size: u8) -> Result<H264Decoder> {
        if parameter_sets.len() < 2 {
            return Err(Error::Invalid("an SPS and a PPS are needed".into()));
        }
        let ptrs: Vec<*const u8> = parameter_sets.iter().map(|s| s.as_ptr()).collect();
        let sizes: Vec<usize> = parameter_sets.iter().map(|s| s.len()).collect();
        let mut format = ptr::null_mut();
        // SAFETY: `ptrs` and `sizes` describe `parameter_sets`, live for the call; `format` is
        // written on success.
        let rc = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                ptrs.len(),
                ptrs.as_ptr(),
                sizes.as_ptr(),
                i32::from(nal_length_size),
                &mut format,
            )
        };
        check("CMVideoFormatDescriptionCreateFromH264ParameterSets", rc)?;
        let format = Cf::new("CMVideoFormatDescriptionCreateFromH264ParameterSets", format.cast_const())?;
        let decoded = Box::new(Decoded { pictures: Mutex::new(Vec::new()) });
        let record = DecompressionCallbackRecord { callback: on_decoded, refcon: (&*decoded as *const Decoded).cast_mut().cast() };
        let mut session = ptr::null_mut();
        // SAFETY: `format` is live; null specifications are allowed; the record is copied and its
        // refcon outlives the session.
        let rc = unsafe { VTDecompressionSessionCreate(ptr::null(), format.0.cast_mut(), ptr::null(), ptr::null(), &record, &mut session) };
        check("VTDecompressionSessionCreate", rc)?;
        if session.is_null() {
            return Err(Error::Service { op: "VTDecompressionSessionCreate", message: "no session".into() });
        }
        Ok(H264Decoder { session, format, decoded })
    }

    /// Decodes one sample (length-prefixed NAL units) and returns its picture's size.
    pub fn decode(&mut self, sample: &[u8]) -> Result<(u32, u32)> {
        if sample.is_empty() {
            return Err(Error::Invalid("an empty sample".into()));
        }
        let mut block = ptr::null_mut();
        // SAFETY: a null memory block with null allocators asks CoreMedia to allocate `len` bytes;
        // `block` is written on success.
        let rc = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(),
                sample.len(),
                ptr::null(),
                ptr::null(),
                0,
                sample.len(),
                0,
                &mut block,
            )
        };
        check("CMBlockBufferCreateWithMemoryBlock", rc)?;
        let block = Cf::new("CMBlockBufferCreateWithMemoryBlock", block.cast_const())?;
        // SAFETY: the block holds `sample.len()` bytes; `sample` is live for the call.
        check("CMBlockBufferReplaceDataBytes", unsafe {
            CMBlockBufferReplaceDataBytes(sample.as_ptr().cast(), block.0.cast_mut(), 0, sample.len())
        })?;
        let size = sample.len();
        let mut buffer = ptr::null_mut();
        // SAFETY: the block and format are live; one sample of `size` bytes and no timing.
        let rc = unsafe {
            CMSampleBufferCreateReady(ptr::null(), block.0.cast_mut(), self.format.0.cast_mut(), 1, 0, ptr::null(), 1, &size, &mut buffer)
        };
        check("CMSampleBufferCreateReady", rc)?;
        let buffer = Cf::new("CMSampleBufferCreateReady", buffer.cast_const())?;
        let mut info = 0u32;
        // SAFETY: the session and sample buffer are live; flags 0 decode synchronously.
        check("VTDecompressionSessionDecodeFrame", unsafe {
            VTDecompressionSessionDecodeFrame(self.session, buffer.0.cast_mut(), 0, ptr::null_mut(), &mut info)
        })?;
        // SAFETY: the session is live.
        check("VTDecompressionSessionWaitForAsynchronousFrames", unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session) })?;
        let mut pictures = self.decoded.pictures.lock().map_err(|_| Error::Invalid("poisoned".into()))?;
        match pictures.drain(..).next_back() {
            Some(Ok(size)) => Ok(size),
            Some(Err(rc)) => Err(Error::Service { op: "VTDecompressionOutputCallback", message: format!("OSStatus {rc}") }),
            None => Err(Error::Service { op: "VTDecompressionSessionDecodeFrame", message: "no picture came out".into() }),
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // SAFETY: the session is live; invalidating it stops every callback before `decoded` is
        // freed, and its one reference is released last.
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            VTDecompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast_const());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmtime_round_trips_microseconds() {
        let t = CMTime::from_duration(Duration::from_nanos(1_234_567_891));
        assert_eq!((t.value, t.timescale, t.flags), (1_234_567_891, TIMESCALE, CMTIME_VALID));
        assert_eq!(t.to_duration(), Some(Duration::from_nanos(1_234_567_891)));
        assert_eq!(CMTime::INVALID.to_duration(), None);
        let ninety_k = CMTime { value: 90_000, timescale: 90_000, flags: CMTIME_VALID, epoch: 0 };
        assert_eq!(ninety_k.to_duration(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn a_pool_refuses_short_frames() {
        let pool = PixelBufferPool::new(32, 16, PixelFormat::Bgra).expect("pool");
        assert!(pool.fill(&[0; 32 * 4 * 15], 32 * 4).is_err(), "one row short");
        assert!(pool.fill(&[0; 32 * 4 * 16], 31 * 4).is_err(), "stride below a row");
        let buf = pool.fill(&[7; 32 * 4 * 16], 32 * 4).expect("fill");
        assert_eq!((buf.size(), &buf.fourcc()), ((32, 16), b"BGRA"));
        let rgba = PixelBufferPool::new(30, 4, PixelFormat::Rgba).expect("pool");
        // A wider source stride than the frame's rows, too.
        let buf = rgba.fill(&[9; 33 * 4 * 4], 33 * 4).expect("fill");
        assert_eq!((buf.size(), &buf.fourcc()), ((30, 4), b"BGRA"));
    }
}
