//! Linux cameras: every `/dev/video*` node that can capture video, described by
//! `VIDIOC_QUERYCAP` and its formats by `VIDIOC_ENUM_FMT`, `VIDIOC_ENUM_FRAMESIZES` and
//! `VIDIOC_ENUM_FRAMEINTERVALS`. v4l2loopback devices (and the vivid and akvcam test drivers) are
//! marked virtual. Opening a node for these ioctls doesn't start a capture.
//!
//! Capture is [`Node`], the ioctl layer under `stream`'s streaming loop: `S_FMT`/`G_FMT`,
//! `S_PARM`, `REQBUFS` with mmap buffers, `QBUF`/`DQBUF` around `poll`, `STREAMON`/`OFF`.
//! Hot-plug is inotify on `/dev` ([`watch`]).
#![allow(unsafe_code)]

use std::ffi::{c_ulong, c_void};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use crate::catalog::{Format, HostCamera, classify_v4l2};
use crate::convert::fourcc_text as fourcc;
use crate::error::{Error, Result};
use crate::frame::FrameSpec;
use crate::hotplug::{Signals, inotify_names_video};
use crate::select::{Interval, Mode, Rates};
use crate::stream::{Dequeued, Device, Negotiated, Offer, Stream, classify};
use crate::tcc::Authorization;

#[cfg(not(target_pointer_width = "64"))]
compile_error!("sim-camera's V4L2 structs (v4l2_format, v4l2_buffer) are laid out for 64-bit Linux; 32-bit kernels use smaller ones");

const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;
const FRMIVAL_TYPE_DISCRETE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const BUF_FLAG_ERROR: u32 = 0x0000_0040;

/// `struct v4l2_capability`.
#[repr(C)]
#[derive(Default)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_fmtdesc`.
#[repr(C)]
#[derive(Default)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_frmsizeenum`; the union is the stepwise form's six words (the discrete form is
/// its first two).
#[repr(C)]
#[derive(Default)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    kind: u32,
    size: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_frmivalenum`; the union is the stepwise form's three fractions.
#[repr(C)]
#[derive(Default)]
struct FrmIvalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    kind: u32,
    interval: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_pix_format`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`'s 200-byte union, 8-byte aligned (it holds `v4l2_window`'s pointers).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct FormatUnion {
    pix: PixFormat,
    rest: [u8; 152],
}

/// `struct v4l2_format`.
#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Format {
    kind: u32,
    fmt: FormatUnion,
}

impl V4l2Format {
    fn capture() -> V4l2Format {
        V4l2Format { kind: BUF_TYPE_VIDEO_CAPTURE, fmt: FormatUnion { pix: PixFormat::default(), rest: [0; 152] } }
    }
}

/// `struct v4l2_requestbuffers`.
#[repr(C)]
#[derive(Default)]
struct RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct timeval` (64-bit).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timeval {
    sec: i64,
    usec: i64,
}

/// `struct v4l2_timecode`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timecode {
    kind: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

/// `struct v4l2_buffer`; `m` is the offset/userptr/planes/fd union, whose `__u32 offset` is its
/// first four bytes.
#[repr(C)]
#[derive(Default)]
struct Buffer {
    index: u32,
    kind: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: Timeval,
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    m: [u8; 8],
    length: u32,
    reserved2: u32,
    request_fd: u32,
}

impl Buffer {
    fn mmap(index: u32) -> Buffer {
        Buffer { index, kind: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, ..Buffer::default() }
    }

    fn offset(&self) -> u32 {
        u32::from_ne_bytes([self.m[0], self.m[1], self.m[2], self.m[3]])
    }
}

/// `struct v4l2_fract`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Fract {
    numerator: u32,
    denominator: u32,
}

/// `struct v4l2_captureparm`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

/// `struct v4l2_streamparm` (its union is 200 bytes, 4-byte aligned).
#[repr(C)]
#[derive(Clone, Copy)]
struct StreamParm {
    kind: u32,
    capture: CaptureParm,
    rest: [u8; 160],
}

const _: () = assert!(size_of::<Capability>() == 104);
const _: () = assert!(size_of::<PixFormat>() == 48);
const _: () = assert!(size_of::<V4l2Format>() == 208);
const _: () = assert!(size_of::<RequestBuffers>() == 20);
const _: () = assert!(size_of::<Buffer>() == 88);
const _: () = assert!(std::mem::offset_of!(Buffer, m) == 64);
const _: () = assert!(size_of::<StreamParm>() == 204);
const _: () = assert!(size_of::<FmtDesc>() == 64);
const _: () = assert!(size_of::<FrmSizeEnum>() == 44);
const _: () = assert!(size_of::<FrmIvalEnum>() == 52);

/// `_IOC(dir, 'V', nr, size)` for the asm-generic layout (x86_64, aarch64).
const fn ioc(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
    dir << 30 | (size as c_ulong) << 16 | (b'V' as c_ulong) << 8 | nr
}
const WRITE: c_ulong = 1;
const READ: c_ulong = 2;
const READ_WRITE: c_ulong = 3;
const VIDIOC_QUERYCAP: c_ulong = ioc(READ, 0, size_of::<Capability>());
const VIDIOC_ENUM_FMT: c_ulong = ioc(READ_WRITE, 2, size_of::<FmtDesc>());
const VIDIOC_ENUM_FRAMESIZES: c_ulong = ioc(READ_WRITE, 74, size_of::<FrmSizeEnum>());
const VIDIOC_ENUM_FRAMEINTERVALS: c_ulong = ioc(READ_WRITE, 75, size_of::<FrmIvalEnum>());
const VIDIOC_G_FMT: c_ulong = ioc(READ_WRITE, 4, size_of::<V4l2Format>());
const VIDIOC_S_FMT: c_ulong = ioc(READ_WRITE, 5, size_of::<V4l2Format>());
const VIDIOC_REQBUFS: c_ulong = ioc(READ_WRITE, 8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: c_ulong = ioc(READ_WRITE, 9, size_of::<Buffer>());
const VIDIOC_QBUF: c_ulong = ioc(READ_WRITE, 15, size_of::<Buffer>());
const VIDIOC_DQBUF: c_ulong = ioc(READ_WRITE, 17, size_of::<Buffer>());
const VIDIOC_STREAMON: c_ulong = ioc(WRITE, 18, size_of::<i32>());
const VIDIOC_STREAMOFF: c_ulong = ioc(WRITE, 19, size_of::<i32>());
const VIDIOC_S_PARM: c_ulong = ioc(READ_WRITE, 22, size_of::<StreamParm>());

/// Issues one ioctl whose argument is `T`, retrying when a signal interrupts it.
fn xioctl<T>(file: &File, request: c_ulong, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `request` encodes `size_of::<T>()` and `T` is the `#[repr(C)]` struct the
        // request reads and writes; `arg` is a valid, exclusive pointer for the call and the fd is
        // open.
        if unsafe { libc::ioctl(file.as_raw_fd(), request as _, std::ptr::from_mut(arg)) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// [`xioctl`] for enumerations: false when the driver refuses (the end of the list, or an
/// unsupported request).
fn ioctl<T>(file: &File, request: c_ulong, arg: &mut T) -> bool {
    xioctl(file, request, arg).is_ok()
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

pub(crate) fn cameras() -> Result<Vec<HostCamera>> {
    let mut nodes: Vec<(u32, String)> = match std::fs::read_dir("/dev") {
        Ok(dir) => dir
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|n| Some((n.strip_prefix("video")?.parse().ok()?, n)))
            .collect(),
        Err(_) => return Ok(Vec::new()),
    };
    nodes.sort_unstable();
    let (mut cameras, mut denied) = (Vec::new(), 0usize);
    for (_, node) in &nodes {
        let path = format!("/dev/{node}");
        let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                denied += 1;
                continue;
            }
            Err(_) => continue,
        };
        if let Some(camera) = describe(&file, &path) {
            cameras.push(camera);
        }
    }
    if cameras.is_empty() && denied > 0 {
        return Err(Error::Permission {
            status: Authorization::Denied,
            fix: format!(
                "{denied} /dev/video* node(s) can't be opened by this user: add it to the device's group \
                 (usually `sudo usermod -aG video $USER`, then log in again)"
            ),
        });
    }
    // Two capture nodes on one bus (rare) still get distinct ids.
    for i in 1..cameras.len() {
        let id = cameras[i].id.clone();
        let n = cameras[..i].iter().filter(|c| c.id == id || c.id.starts_with(&format!("{id}#"))).count();
        if n > 0 {
            cameras[i].id = format!("{id}#{n}");
        }
    }
    Ok(cameras)
}

fn describe(file: &File, path: &str) -> Option<HostCamera> {
    let mut cap = Capability::default();
    if !ioctl(file, VIDIOC_QUERYCAP, &mut cap) {
        return None;
    }
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
    if caps & (CAP_VIDEO_CAPTURE | CAP_VIDEO_CAPTURE_MPLANE) == 0 {
        return None; // a metadata or output node
    }
    let (driver, card, bus) = (text(&cap.driver), text(&cap.card), text(&cap.bus_info));
    let (kind, is_virtual) = classify_v4l2(&driver, &card, &bus);
    Some(HostCamera {
        id: if bus.is_empty() { path.to_string() } else { bus },
        name: if card.is_empty() { path.to_string() } else { card },
        kind,
        is_virtual,
        manufacturer: None,
        model: None,
        transport: Some(driver),
        path: Some(path.to_string()),
        formats: formats(file),
    })
}

fn formats(file: &File) -> Vec<Format> {
    let mut out = Vec::new();
    for index in 0..64 {
        let mut desc = FmtDesc { index, kind: BUF_TYPE_VIDEO_CAPTURE, ..FmtDesc::default() };
        if !ioctl(file, VIDIOC_ENUM_FMT, &mut desc) {
            break;
        }
        for size_index in 0..64 {
            let mut size = FrmSizeEnum { index: size_index, pixel_format: desc.pixelformat, ..FrmSizeEnum::default() };
            if !ioctl(file, VIDIOC_ENUM_FRAMESIZES, &mut size) {
                break;
            }
            // Stepwise and continuous ranges (min, max, step of width, then of height) report
            // their maximum.
            let (width, height) =
                if size.kind == FRMSIZE_TYPE_DISCRETE { (size.size[0], size.size[1]) } else { (size.size[1], size.size[4]) };
            let max_fps = max_fps(file, desc.pixelformat, width, height);
            out.push(Format { width, height, pixel_format: fourcc(desc.pixelformat), max_fps });
            if size.kind != FRMSIZE_TYPE_DISCRETE {
                break;
            }
        }
    }
    out
}

fn max_fps(file: &File, pixel_format: u32, width: u32, height: u32) -> Option<f64> {
    let mut best: Option<f64> = None;
    for index in 0..64 {
        let mut ival = FrmIvalEnum { index, pixel_format, width, height, ..FrmIvalEnum::default() };
        if !ioctl(file, VIDIOC_ENUM_FRAMEINTERVALS, &mut ival) {
            break;
        }
        // Discrete: one fraction (seconds per frame); stepwise: min, max, step: the min is fastest.
        let [num, den, ..] = ival.interval;
        if num > 0 {
            let fps = f64::from(den) / f64::from(num);
            best = Some(best.map_or(fps, |b: f64| b.max(fps)));
        }
        if ival.kind != FRMIVAL_TYPE_DISCRETE {
            break;
        }
    }
    best
}

/// The rates a format runs at at one size: each discrete interval, or one stepwise range.
fn rates(file: &File, pixel_format: u32, width: u32, height: u32) -> Vec<Rates> {
    let mut out = Vec::new();
    for index in 0..64 {
        let mut ival = FrmIvalEnum { index, pixel_format, width, height, ..FrmIvalEnum::default() };
        if !ioctl(file, VIDIOC_ENUM_FRAMEINTERVALS, &mut ival) {
            break;
        }
        // Discrete: one fraction; stepwise and continuous: min, max, step.
        let fract = |i: usize| Interval { num: u64::from(ival.interval[i]), den: u64::from(ival.interval[i + 1]) };
        if ival.kind == FRMIVAL_TYPE_DISCRETE {
            out.push(Rates::one(fract(0)));
        } else {
            out.push(Rates { fastest: fract(0), slowest: fract(2) });
            break;
        }
    }
    out.retain(|r| r.fastest.num > 0 && r.fastest.den > 0 && r.slowest.num > 0 && r.slowest.den > 0);
    out
}

/// `want` fitted into a stepwise range: clamped, then down to a step.
fn fit_step(want: u32, min: u32, max: u32, step: u32) -> u32 {
    let v = want.clamp(min, max.max(min));
    if step > 1 { min + (v - min) / step * step } else { v }
}

/// One mmap'd capture buffer.
struct Mapping {
    ptr: *mut c_void,
    len: usize,
}

/// An open `/dev/video*` node streaming through mmap buffers: [`Device`] for the stream loop.
pub(crate) struct Node {
    file: File,
    path: String,
    buffers: Vec<Mapping>,
    requested: bool,
}

// SAFETY: the mappings belong to this node alone and are only read through `&self` by its owner
// (the capture thread); the fd and the mappings may be used from any thread.
unsafe impl Send for Node {}

impl Node {
    /// `err`, or `ENODEV` when the node has gone from `/dev` (some drivers fail an unplugged
    /// device's calls with `EIO` or `EINVAL`).
    fn gone_or(&self, err: io::Error) -> io::Error {
        if Path::new(&self.path).exists() { err } else { io::Error::from_raw_os_error(libc::ENODEV) }
    }

    fn call<T>(&self, request: c_ulong, arg: &mut T) -> io::Result<()> {
        xioctl(&self.file, request, arg).map_err(|e| self.gone_or(e))
    }
}

impl Device for Node {
    fn offers(&mut self, want: FrameSpec) -> io::Result<Vec<Offer>> {
        let mut out = Vec::new();
        for index in 0..64 {
            let mut desc = FmtDesc { index, kind: BUF_TYPE_VIDEO_CAPTURE, ..FmtDesc::default() };
            match self.call(VIDIOC_ENUM_FMT, &mut desc) {
                Ok(()) => {}
                // EINVAL ends the list; anything else on the first call is the node's failure.
                Err(e) if index == 0 && e.raw_os_error() != Some(libc::EINVAL) => return Err(e),
                Err(_) => break,
            }
            for size_index in 0..64 {
                let mut size = FrmSizeEnum { index: size_index, pixel_format: desc.pixelformat, ..FrmSizeEnum::default() };
                if !ioctl(&self.file, VIDIOC_ENUM_FRAMESIZES, &mut size) {
                    break;
                }
                let s = size.size;
                let sizes = if size.kind == FRMSIZE_TYPE_DISCRETE {
                    vec![(s[0], s[1])]
                } else {
                    // Stepwise and continuous: min, max, step of width, then of height. The largest,
                    // and the request fitted into the range.
                    vec![(s[1], s[4]), (fit_step(want.width, s[0], s[1], s[2]), fit_step(want.height, s[3], s[4], s[5]))]
                };
                for (width, height) in sizes {
                    let rates = rates(&self.file, desc.pixelformat, width, height);
                    out.push(Offer { fourcc: desc.pixelformat, mode: Mode { width, height, rank: 0, rates } });
                }
                if size.kind != FRMSIZE_TYPE_DISCRETE {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn set_format(&mut self, fourcc: u32, width: u32, height: u32) -> io::Result<Negotiated> {
        let mut set = V4l2Format::capture();
        (set.fmt.pix.width, set.fmt.pix.height, set.fmt.pix.pixelformat) = (width, height, fourcc);
        self.call(VIDIOC_S_FMT, &mut set)?;
        let mut got = V4l2Format::capture();
        self.call(VIDIOC_G_FMT, &mut got)?;
        let pix = got.fmt.pix;
        Ok(Negotiated { fourcc: pix.pixelformat, width: pix.width, height: pix.height, stride: pix.bytesperline })
    }

    fn set_interval(&mut self, interval: Interval) -> io::Result<()> {
        let invalid = || io::Error::from_raw_os_error(libc::EINVAL);
        let mut parm = StreamParm { kind: BUF_TYPE_VIDEO_CAPTURE, capture: CaptureParm::default(), rest: [0; 160] };
        parm.capture.timeperframe = Fract {
            numerator: u32::try_from(interval.num).map_err(|_| invalid())?,
            denominator: u32::try_from(interval.den).map_err(|_| invalid())?,
        };
        self.call(VIDIOC_S_PARM, &mut parm)
    }

    fn request_buffers(&mut self, count: u32) -> io::Result<u32> {
        let mut req = RequestBuffers { count, kind: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, ..RequestBuffers::default() };
        self.call(VIDIOC_REQBUFS, &mut req)?;
        self.requested = true;
        Ok(req.count)
    }

    fn map(&mut self, index: u32) -> io::Result<()> {
        let mut buf = Buffer::mmap(index);
        self.call(VIDIOC_QUERYBUF, &mut buf)?;
        let len = buf.length as usize;
        // SAFETY: maps `len` bytes of the driver's buffer `index` at the offset QUERYBUF gave, shared
        // with the driver; unmapped in `drop`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                buf.offset() as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.buffers.push(Mapping { ptr, len });
        Ok(())
    }

    fn queue(&mut self, index: u32) -> io::Result<()> {
        self.call(VIDIOC_QBUF, &mut Buffer::mmap(index))
    }

    fn stream_on(&mut self) -> io::Result<()> {
        self.call(VIDIOC_STREAMON, &mut (BUF_TYPE_VIDEO_CAPTURE as i32))
    }

    fn stream_off(&mut self) -> io::Result<()> {
        self.call(VIDIOC_STREAMOFF, &mut (BUF_TYPE_VIDEO_CAPTURE as i32))
    }

    fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
        let mut pfd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd for the call.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(self.gone_or(err)) };
        }
        if n == 0 {
            return Ok(false);
        }
        if pfd.revents & libc::POLLIN != 0 {
            return Ok(true);
        }
        Err(self.gone_or(io::Error::other("poll reported an error on the device")))
    }

    fn dequeue(&mut self) -> io::Result<Option<Dequeued>> {
        let mut buf = Buffer::mmap(0);
        match xioctl(&self.file, VIDIOC_DQBUF, &mut buf) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(None),
            Err(e) => return Err(self.gone_or(e)),
        }
        let Some(mapping) = self.buffers.get(buf.index as usize) else {
            return Err(io::Error::other(format!("the driver returned buffer {} of {}", buf.index, self.buffers.len())));
        };
        Ok(Some(Dequeued { index: buf.index, used: (buf.bytesused as usize).min(mapping.len), damaged: buf.flags & BUF_FLAG_ERROR != 0 }))
    }

    fn data(&self, index: u32, len: usize) -> &[u8] {
        let Some(m) = self.buffers.get(index as usize) else { return &[] };
        // SAFETY: the mapping is `m.len` readable bytes for as long as the node lives, and buffer
        // `index` is dequeued (the driver doesn't write it) while the stream loop reads it.
        unsafe { std::slice::from_raw_parts(m.ptr.cast::<u8>(), len.min(m.len)) }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if self.requested {
            let _ = self.stream_off();
        }
        for m in self.buffers.drain(..) {
            // SAFETY: a mapping `map` made and nothing else unmaps.
            unsafe { libc::munmap(m.ptr, m.len) };
        }
        if self.requested {
            // Frees the driver's buffers (it refuses while they are mapped, hence after munmap).
            let _ = xioctl(
                &self.file,
                VIDIOC_REQBUFS,
                &mut RequestBuffers { kind: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, ..RequestBuffers::default() },
            );
        }
    }
}

/// Opens `camera`'s node and streams from it in the mode nearest `spec`.
pub(crate) fn open(camera: &HostCamera, spec: FrameSpec) -> Result<Stream> {
    let path = camera.path.clone().ok_or_else(|| Error::Unsupported(format!("{} has no device node to capture from", camera.name)))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| classify(&e, &format!("opening {path}"), &camera.name))?;
    Stream::open(Node { file, path, buffers: Vec::new(), requested: false }, &camera.name, spec)
}

/// inotify on `/dev`: nodes created, deleted, renamed, or given new permissions (udev opens a new
/// node up to its group after creating it).
pub(crate) struct DevWatch {
    fd: OwnedFd,
}

pub(crate) fn watch() -> Result<Box<dyn Signals>> {
    let failed = |what: &str| Error::Io(format!("{what}: {}", io::Error::last_os_error()));
    // SAFETY: takes flags only; returns a new descriptor or -1.
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(failed("inotify_init1"));
    }
    // SAFETY: `raw` is a fresh descriptor nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mask = libc::IN_CREATE | libc::IN_DELETE | libc::IN_ATTRIB | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
    // SAFETY: a valid inotify descriptor and a NUL-terminated path.
    if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c"/dev".as_ptr(), mask) } < 0 {
        return Err(failed("watching /dev"));
    }
    Ok(Box::new(DevWatch { fd }))
}

impl Signals for DevWatch {
    fn wait(&mut self, timeout: Duration) -> Result<bool> {
        let mut pfd = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd for the call.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(Error::Io(format!("inotify poll: {err}"))) };
        }
        let mut seen = false;
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: reads at most `buf.len()` bytes into `buf`.
            let read = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if read <= 0 {
                break; // EAGAIN: drained
            }
            seen |= inotify_names_video(&buf[..read as usize]);
        }
        Ok(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
    }

    #[test]
    fn stepwise_sizes_fit_the_request_to_a_step() {
        assert_eq!(fit_step(1000, 16, 1920, 8), 1000);
        assert_eq!(fit_step(1001, 16, 1920, 8), 1000);
        assert_eq!(fit_step(4000, 16, 1920, 8), 1920);
        assert_eq!(fit_step(1, 16, 1920, 1), 16);
    }

    #[test]
    fn buffer_offsets_read_the_unions_first_word() {
        let mut buf = Buffer::mmap(3);
        buf.m[..4].copy_from_slice(&0x1234_5000u32.to_ne_bytes());
        assert_eq!((buf.index, buf.offset()), (3, 0x1234_5000));
    }

    #[test]
    fn strings_and_fourccs_decode() {
        assert_eq!(text(b"uvcvideo\0\0\0junk"), "uvcvideo");
        assert_eq!(fourcc(u32::from_le_bytes(*b"YUYV")), "YUYV");
        assert_eq!(fourcc(1), "????");
    }
}
