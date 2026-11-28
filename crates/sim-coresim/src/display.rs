//! The device's framebuffer: a shared surface the simulated OS renders into.

use std::ffi::c_void;

use block2::{Block, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{class, msg_send, sel};
use sim_core::frame::Frame;
use sim_core::framebuffer::{self, Port};
use sim_core::pixels::bgra_to_rgba;
use sim_core::shot::{Stamp, hash_rows};

use crate::error::{Error, Result};
use crate::rt::{self, Obj};

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(surface: *const AnyObject, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *const AnyObject, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *const AnyObject) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetWidth(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetHeight(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetAllocSize(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *const AnyObject) -> u32;
    fn IOSurfaceGetSeed(surface: *const AnyObject) -> u32;
    fn IOSurfaceGetID(surface: *const AnyObject) -> u32;
}

const LOCK_READ_ONLY: u32 = 1;
const FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// A retained handle to the framebuffer `IOSurface`, for zero-copy display. The simulated device
/// keeps rendering into it, so readers see live (possibly mid-update) contents.
pub struct Surface(Obj);

// SAFETY: an IOSurface is a thread-safe CF object; this handle only retains it and reads its
// properties.
unsafe impl Send for Surface {}
// SAFETY: as above.
unsafe impl Sync for Surface {}

impl Surface {
    /// Takes over a retained IOSurface.
    pub(crate) fn from_retained(surface: Obj) -> Surface {
        Surface(surface)
    }

    /// The raw `IOSurfaceRef`, valid while this handle lives.
    pub fn as_raw(&self) -> *mut c_void {
        Retained::as_ptr(&self.0).cast_mut().cast()
    }

    /// A counter that changes whenever the contents change.
    pub fn seed(&self) -> u32 {
        // SAFETY: `self.0` is a live, retained IOSurface.
        unsafe { IOSurfaceGetSeed(&*self.0) }
    }

    /// Size in pixels.
    pub fn size(&self) -> (u32, u32) {
        // SAFETY: `self.0` is a live, retained IOSurface.
        unsafe { (IOSurfaceGetWidth(&*self.0) as u32, IOSurfaceGetHeight(&*self.0) as u32) }
    }

    /// The pixel format, as its four characters (`BGRA`).
    pub fn pixel_format(&self) -> [u8; 4] {
        // SAFETY: `self.0` is a live, retained IOSurface.
        unsafe { IOSurfaceGetPixelFormat(&*self.0) }.to_be_bytes()
    }
}

/// A device display backed by a framebuffer surface.
pub struct Display {
    descriptor: Obj,
}

impl Display {
    /// Finds the main display among the device's I/O ports: the class-0 framebuffer (with several,
    /// as on the iPhone Duo, the one the size of the device type's main `screen`), else the first
    /// one (Apple TV renders only to its TVOut port). See [`sim_core::framebuffer::main_port`].
    pub(crate) fn main(device: &AnyObject, screen: Option<(u32, u32)>) -> Result<Self> {
        Self::builtins(device, screen).map(|(main, _)| main)
    }

    /// The main display and, on a device with two built-in screens (the iPhone Duo), the other
    /// one: [`sim_core::framebuffer::inner_port`].
    pub(crate) fn builtins(device: &AnyObject, screen: Option<(u32, u32)>) -> Result<(Self, Option<Self>)> {
        let ports = framebuffer_ports(device)?;
        let classes: Vec<Option<u16>> = ports.iter().map(|d| display_class(d)).collect();
        let sizes = framebuffer::needs_sizes(&classes);
        let described: Vec<Port> = ports
            .iter()
            .zip(&classes)
            .map(|(d, &class)| Port {
                class,
                size: if sizes && class == Some(0) { Self { descriptor: d.clone() }.size().ok() } else { None },
            })
            .collect();
        let at = |i: Option<usize>| i.and_then(|i| ports.get(i)).map(|d| Self { descriptor: d.clone() });
        let main = at(framebuffer::main_port(&described, screen))
            .ok_or_else(|| Error::NoDisplay("no framebuffer port; is the device booted?".into()))?;
        Ok((main, at(framebuffer::inner_port(&described, screen))))
    }

    /// The screen's ID (`screenProperties.screenID`, 1 for the main screen), where the descriptor
    /// is a screen (CoreSimulator 1051 on): what its digitizer is addressed by.
    pub fn screen_id(&self) -> Option<u32> {
        if !rt::responds(&self.descriptor, sel!(screenProperties)) {
            return None;
        }
        let props = rt::object_prop(&self.descriptor, sel!(screenProperties)).ok()?;
        // SAFETY: `screenID` returns an unsigned 32-bit integer.
        unsafe { rt::send_scalar(&props, sel!(screenID)) }
    }

    /// Whether the screen shows anything ([`sim_core::duo::lit`] on a sparse grid): a built-in
    /// screen not in use is exactly black. `false` without a surface.
    pub fn lit(&self) -> bool {
        /// One pixel in this many, each way: 1/256 of a screen, a few thousand reads.
        const STEP: usize = 16;
        let Some(surface) = self.surface() else { return false };
        let s: *const AnyObject = &*surface;
        // SAFETY: `s` is a live IOSurface (retained by `surface`) for all calls below.
        if unsafe { IOSurfaceGetPixelFormat(s) } != FORMAT_BGRA || unsafe { IOSurfaceLock(s, LOCK_READ_ONLY, std::ptr::null_mut()) } != 0 {
            return false;
        }
        // SAFETY: the surface is locked, so its geometry and base address are stable.
        let (width, height, stride, len, base) = unsafe {
            (IOSurfaceGetWidth(s), IOSurfaceGetHeight(s), IOSurfaceGetBytesPerRow(s), IOSurfaceGetAllocSize(s), IOSurfaceGetBaseAddress(s))
        };
        // SAFETY: while locked, `base` points to `len` readable bytes owned by the surface; the
        // slice does not outlive the lock.
        let lit = !base.is_null()
            && sim_core::duo::lit(unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) }, width, height, stride, STEP);
        // SAFETY: balances the successful lock above.
        unsafe { IOSurfaceUnlock(s, LOCK_READ_ONLY, std::ptr::null_mut()) };
        lit
    }

    /// Finds an external display (an iPhone's TVOut screen, the CarPlay display once attached):
    /// a framebuffer that isn't class 0 and has rendered a surface.
    pub(crate) fn external(device: &AnyObject) -> Result<Self> {
        framebuffer_ports(device)?
            .into_iter()
            .find(|d| display_class(d).is_some_and(|c| c != 0) && rt::object_prop(d, sel!(framebufferSurface)).is_ok())
            .map(|descriptor| Self { descriptor })
            .ok_or_else(|| Error::NoDisplay("no external display has rendered yet; attach CarPlay first".into()))
    }

    /// The current framebuffer surface (an `IOSurface`), if the device has rendered one.
    pub fn surface(&self) -> Option<Obj> {
        rt::object_prop(&self.descriptor, sel!(framebufferSurface)).ok()
    }

    /// A retained handle to the current framebuffer surface, if the device has rendered one.
    pub fn surface_handle(&self) -> Option<Surface> {
        self.surface().map(Surface)
    }

    /// A counter that changes whenever the surface contents change. 0 without a surface.
    pub fn seed(&self) -> u32 {
        // SAFETY: the surface is a live IOSurface for the duration of the call.
        self.surface().map_or(0, |s| unsafe { IOSurfaceGetSeed(&*s) })
    }

    /// Framebuffer size in pixels.
    pub fn size(&self) -> Result<(u32, u32)> {
        let surface = self.surface().ok_or_else(|| Error::NoDisplay("no framebuffer surface yet".into()))?;
        // SAFETY: the surface is a live IOSurface for the duration of the calls.
        let (w, h) = unsafe { (IOSurfaceGetWidth(&*surface), IOSurfaceGetHeight(&*surface)) };
        Ok((w as u32, h as u32))
    }

    /// Calls `on_frame` whenever the device presents a frame, until the returned watch drops. It is
    /// the port's damage-rectangle callback (Xcode 9 on; modern runtimes call it once per frame,
    /// with no rectangles), what idb's lazy video stream follows. `on_frame` runs on a service
    /// thread while the service waits: keep it to a flag and a wakeup.
    pub fn watch_frames(&self, on_frame: impl Fn() + Send + Sync + 'static) -> Result<FrameWatch> {
        let register = sel!(registerCallbackWithUUID:damageRectanglesCallback:);
        rt::require(&self.descriptor, register)?;
        rt::require(&self.descriptor, sel!(unregisterDamageRectanglesCallbackWithUUID:))?;
        // SAFETY: `+[NSUUID UUID]` returns a new random UUID.
        let uuid: Obj = unsafe { msg_send![class!(NSUUID), UUID] };
        let block = RcBlock::with_encoding::<_, _, _, rt::ObjBlock>(move |_rects: *mut AnyObject| on_frame());
        type RegisterFn = unsafe extern "C" fn(*const AnyObject, Sel, *const AnyObject, *const Block<dyn Fn(*mut AnyObject)>);
        // SAFETY: the method's signature is (NSUUID *, void (^)(NSArray *)) -> void; the descriptor
        // is a remote proxy, so it's called through `objc_msgSend`, not `msg_send!`.
        let send: RegisterFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: the descriptor responds to the selector (checked); the service copies the block,
        // and the watch keeps ours alive until it unregisters.
        unsafe { send(&*self.descriptor, register, &*uuid, &*block) };
        Ok(FrameWatch { descriptor: self.descriptor.clone(), uuid, _block: block })
    }

    /// The current surface's ID, seed and size, from one fetch of it (no pixels are read): what
    /// the screenshot cache ([`sim_core::shot`]) checks before it reads anything.
    pub fn stamp(&self) -> Result<Stamp> {
        let surface = self.surface().ok_or_else(|| Error::NoDisplay("no framebuffer surface yet".into()))?;
        let s: *const AnyObject = &*surface;
        // SAFETY: `s` is a live IOSurface (retained by `surface`) for all calls below.
        let (surface, seed, width, height) =
            unsafe { (IOSurfaceGetID(s), IOSurfaceGetSeed(s), IOSurfaceGetWidth(s) as u32, IOSurfaceGetHeight(s) as u32) };
        Ok(Stamp { surface, seed, width, height })
    }

    /// Copies the current framebuffer contents.
    pub fn capture(&self) -> Result<Frame> {
        let frame = |bgra: &[u8], width: usize, height: usize, stride: usize| {
            Some(Frame { width: width as u32, height: height as u32, rgba: bgra_to_rgba(bgra, width, height, stride)? })
        };
        self.read(frame).map(|(frame, _)| frame)
    }

    /// The contents' [`hash_rows`] (of the BGRA rows, read in place: no copy) and the [`Stamp`] of
    /// the surface they were read from (its seed is the lock's), from one fetch and one lock.
    pub fn content_hash(&self) -> Result<(Stamp, u64)> {
        self.read(|bgra, width, height, stride| hash_rows(bgra, width * 4, height, stride)).map(|(hash, stamp)| (stamp, hash))
    }

    /// [`Display::capture`] with the [`Stamp`] of the surface the pixels were read from and their
    /// hash (as [`Display::content_hash`] gives it), from one fetch and one lock.
    pub fn capture_hashed(&self) -> Result<(Frame, Stamp, u64)> {
        let both = |bgra: &[u8], width: usize, height: usize, stride: usize| {
            let hash = hash_rows(bgra, width * 4, height, stride)?;
            Some((Frame { width: width as u32, height: height as u32, rgba: bgra_to_rgba(bgra, width, height, stride)? }, hash))
        };
        self.read(both).map(|((frame, hash), stamp)| (frame, stamp, hash))
    }

    /// Runs `with` on the current surface's BGRA bytes (and its width, height and bytes per row)
    /// while it is locked for reading, with the [`Stamp`] of that same surface (the seed the lock
    /// reported, the ID and size read under it): one surface fetch makes the whole stamp. `None`
    /// from `with` means the geometry is inconsistent.
    fn read<R>(&self, with: impl FnOnce(&[u8], usize, usize, usize) -> Option<R>) -> Result<(R, Stamp)> {
        let surface = self.surface().ok_or_else(|| Error::NoDisplay("no framebuffer surface yet".into()))?;
        let s: *const AnyObject = &*surface;
        // SAFETY: `s` is a live IOSurface (retained by `surface`) for all calls below.
        let format = unsafe { IOSurfaceGetPixelFormat(s) };
        if format != FORMAT_BGRA {
            return Err(Error::NoDisplay(format!("unsupported pixel format {:?}", String::from_utf8_lossy(&format.to_be_bytes()))));
        }
        let mut seed = 0u32;
        // SAFETY: as above; `seed` is a valid out pointer for the call.
        if unsafe { IOSurfaceLock(s, LOCK_READ_ONLY, &mut seed) } != 0 {
            return Err(Error::NoDisplay("cannot lock framebuffer".into()));
        }
        // SAFETY: the surface is locked, so its geometry and base address are stable.
        let (width, height, stride, len, base) = unsafe {
            (IOSurfaceGetWidth(s), IOSurfaceGetHeight(s), IOSurfaceGetBytesPerRow(s), IOSurfaceGetAllocSize(s), IOSurfaceGetBaseAddress(s))
        };
        // SAFETY: as above, the surface is still locked.
        let id = unsafe { IOSurfaceGetID(s) };
        let result = if base.is_null() {
            None
        } else {
            // SAFETY: while locked, `base` points to `len` readable bytes owned by the surface; the
            // slice does not outlive the lock.
            with(unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) }, width, height, stride)
        };
        // SAFETY: balances the successful lock above.
        unsafe { IOSurfaceUnlock(s, LOCK_READ_ONLY, std::ptr::null_mut()) };
        let stamp = Stamp { surface: id, seed, width: width as u32, height: height as u32 };
        result.map(|r| (r, stamp)).ok_or_else(|| Error::NoDisplay("framebuffer geometry is inconsistent".into()))
    }
}

/// A registered frame callback ([`Display::watch_frames`]); dropping it unregisters the callback.
pub struct FrameWatch {
    descriptor: Obj,
    uuid: Obj,
    _block: RcBlock<dyn Fn(*mut AnyObject)>,
}

impl Drop for FrameWatch {
    fn drop(&mut self) {
        type UnregisterFn = unsafe extern "C" fn(*const AnyObject, Sel, *const AnyObject);
        // SAFETY: the method's signature is (NSUUID *) -> void; called through `objc_msgSend` as
        // the descriptor is a remote proxy.
        let send: UnregisterFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: the descriptor responds to the selector (checked at registration); `uuid` is the
        // registration's.
        unsafe { send(&*self.descriptor, sel!(unregisterDamageRectanglesCallbackWithUUID:), &*self.uuid) };
    }
}

/// The descriptors of the device's framebuffer ports, in port order.
fn framebuffer_ports(device: &AnyObject) -> Result<Vec<Obj>> {
    let io = rt::object_prop(device, sel!(io))?;
    if rt::responds(&io, sel!(updateIOPorts)) {
        // SAFETY: `updateIOPorts` takes no arguments and returns void (checked to exist).
        unsafe { msg_send![&*io, updateIOPorts] }
    }
    let ports = rt::object_prop(&io, sel!(ioPorts))?;
    Ok(rt::array_items(&ports)
        .iter()
        .filter_map(|port| rt::object_prop(port, sel!(descriptor)).ok())
        .filter(|d| rt::responds(d, sel!(framebufferSurface)))
        .collect())
}

fn display_class(descriptor: &AnyObject) -> Option<u16> {
    let state = rt::object_prop(descriptor, sel!(state)).ok()?;
    rt::send_u16(&state, sel!(displayClass))
}
