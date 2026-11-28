//! The `-gpu metal` present path (issue #356, docs/research/metal-present.md): a posted frame is
//! copied on the GPU from the renderer's color buffer into an IOSurface, and that surface, not
//! read-back pixels, goes to simee.
//!
//! gfxstream keeps each color buffer's Vulkan memory as a placement `MTLHeap` (private
//! `VkEmulation::getColorBufferMetalMemoryHandle`). A texture made over that heap at offset 0
//! with the image's own description reads the image's bytes ([`MetalGpu::source`]); one compute
//! pass copies it into a pooled BGRA IOSurface (red and blue swapped on the way, since CoreVideo
//! has no IOSurface-backed RGBA buffers), and the CPU waits for it to finish.
//!
//! **Ordering.** The copy runs on a Metal queue of its own, so nothing on the GPU orders it after
//! the compositor. What orders it is the CPU: under host composition `FrameBuffer::compose` waits
//! for the composition's fence (its callback waits on the GPU future, then sets the promise
//! `compose` waits on) before `postImplSync` records the target as the last posted color buffer.
//! So by the time [`crate::Renderer::last_posted_color_buffer`] names a target, every GPU write
//! into it has completed, and a command buffer committed after that reads the finished frame.
//! A target is drawn into again only after the compositor has cycled through the others (three
//! on API 36), and the copy is waited for (0.03–0.1 ms of GPU) before the surface is handed out.
//! A frame the guest posts itself (client composition) is ordered by the guest's own fence wait
//! before its post, as for the readback path.
//!
//! **Lifetime.** The heap handle is unretained and lives as long as its color buffer. The source
//! texture made over it retains the heap, and the source is cached by (color buffer, heap
//! address), so while a heap is cached its address can't name another heap. The one window left
//! is between the renderer handing out the pointer and the retain, a few instructions; the posted
//! targets live as long as the display's configuration.
//!
//! **Recycling.** Surfaces come from a [`Pool`] of [`POOL`] per size, handed out as `Arc`s: one is
//! written again only once every holder has dropped it (the frame store, and each client until it
//! sends `release <seq>`). With every surface held, no surface is made and the frame goes the
//! readback way.
//!
//! The pool and the cache are plain logic over a [`Gpu`], tested with fakes; [`MetalGpu`] is
//! Metal's. [`MachSender`] moves a surface's send right to a client's port.

use std::sync::Arc;

/// Surfaces per size: one for the frame store's latest frame, one a client may be encoding, one
/// being written.
pub const POOL: usize = 3;
/// Source textures kept (one per posted color buffer: three on API 36, plus a resize's).
pub const SOURCES: usize = 8;

/// The GPU work a present needs, so the logic around it can be tested without a GPU.
pub trait Gpu {
    /// A texture over a color buffer's memory, keeping that memory alive.
    type Source;
    /// A pooled surface the copy writes.
    type Target;
    /// The renderer's memory for color buffer `cb`, by address (null: none).
    fn memory(&mut self, cb: u32) -> usize;
    /// A texture over `memory`'s `width` x `height` image, retaining it; `None` when it can't be
    /// made.
    fn source(&mut self, memory: usize, width: u32, height: u32) -> Option<Self::Source>;
    /// A new surface of `width` x `height`.
    fn target(&mut self, width: u32, height: u32) -> Option<Self::Target>;
    /// Copies `src` into `dst` and waits until the GPU is done: false if it failed.
    fn copy(&mut self, src: &Self::Source, dst: &Self::Target, width: u32, height: u32) -> bool;
}

/// Surfaces of one size, each reused only once nobody holds it.
pub struct Pool<T> {
    size: (u32, u32),
    slots: Vec<Arc<T>>,
}

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Pool { size: (0, 0), slots: Vec::new() }
    }
}

impl<T> Pool<T> {
    /// A surface of `size` nobody else holds, made with `make` while the pool has fewer than
    /// [`POOL`]; `None` when all are held. A new size starts the pool over: surfaces of the old
    /// size live on with their holders and go when they're dropped.
    pub fn acquire(&mut self, size: (u32, u32), make: impl FnOnce() -> Option<T>) -> Option<Arc<T>> {
        if size != self.size {
            self.slots.clear();
            self.size = size;
        }
        // Held only here: no one else can clone it while the pool is borrowed.
        if let Some(free) = self.slots.iter().find(|s| Arc::strong_count(s) == 1) {
            return Some(free.clone());
        }
        if self.slots.len() >= POOL {
            return None;
        }
        let made = Arc::new(make()?);
        self.slots.push(made.clone());
        Some(made)
    }

    /// Surfaces made and not yet dropped by the pool.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

struct Cached<S> {
    cb: u32,
    memory: usize,
    size: (u32, u32),
    source: S,
}

/// Copies posted color buffers into pooled surfaces (see the module docs).
pub struct Presenter<G: Gpu> {
    gpu: G,
    /// Most recently used last.
    sources: Vec<Cached<G::Source>>,
    pool: Pool<G::Target>,
}

impl<G: Gpu> Presenter<G> {
    pub fn new(gpu: G) -> Presenter<G> {
        Presenter { gpu, sources: Vec::new(), pool: Pool::default() }
    }

    /// Color buffer `cb` (`width` x `height`) copied into a pooled surface, finished on the GPU.
    /// Call it only for a buffer whose writes are done (a posted one: see the module docs).
    /// `None` when the buffer has no memory to copy from, no surface is free, or the copy failed.
    pub fn present(&mut self, cb: u32, width: u32, height: u32) -> Option<Arc<G::Target>> {
        let memory = self.gpu.memory(cb);
        if memory == 0 {
            return None;
        }
        let size = (width, height);
        let at = match self.sources.iter().position(|s| (s.cb, s.memory, s.size) == (cb, memory, size)) {
            Some(i) => i,
            None => {
                // The buffer's memory or size changed (freed and made again under its handle):
                // its old source goes, and its memory with it.
                self.sources.retain(|s| s.cb != cb);
                let source = self.gpu.source(memory, width, height)?;
                if self.sources.len() >= SOURCES {
                    self.sources.remove(0);
                }
                self.sources.push(Cached { cb, memory, size, source });
                self.sources.len() - 1
            }
        };
        let used = self.sources.remove(at);
        self.sources.push(used);
        let Presenter { gpu, sources, pool } = self;
        let dst = pool.acquire(size, || gpu.target(width, height))?;
        let src = &sources.last()?.source;
        // A failed copy hands nothing out; the surface goes back to the pool with `dst`.
        gpu.copy(src, &dst, width, height).then_some(dst)
    }

    /// Sources cached, for tests.
    pub fn sources(&self) -> usize {
        self.sources.len()
    }

    pub fn pool(&self) -> &Pool<G::Target> {
        &self.pool
    }
}

/// Whether BGRA rows `stride` bytes apart hold the tightly packed RGBA frame `rgba` with red and
/// blue swapped: how a surface is checked against the readback.
pub fn bgra_matches_rgba(bgra: &[u8], stride: usize, rgba: &[u8], width: u32, height: u32) -> bool {
    let row = width as usize * 4;
    if rgba.len() != row * height as usize || (height > 0 && bgra.len() < stride * (height as usize - 1) + row) {
        return false;
    }
    (0..height as usize).all(|y| {
        let (got, want) = (&bgra[y * stride..y * stride + row], &rgba[y * row..y * row + row]);
        got.as_chunks::<4>()
            .0
            .iter()
            .zip(want.as_chunks::<4>().0)
            .all(|(g, w)| g[0] == w[2] && g[1] == w[1] && g[2] == w[0] && g[3] == w[3])
    })
}

/// What a Metal device says it can do, as far as the present path cares ([`present_gate`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `supportsFamily:` `MTLGPUFamilyMetal3`.
    pub metal3: bool,
    /// Placement heaps (`supportsFamily:` Apple2 or Mac2): what gfxstream's color buffers are.
    pub placement_heaps: bool,
    /// This Mac is a virtual machine's guest (`kern.hv_vmm_present`), so its GPU is a virtual one
    /// whatever it is called.
    pub virtual_machine: bool,
    /// The device's name says it is the paravirtualized GPU of a macOS VM ("Apple Paravirtual
    /// device"), which forwards Metal to the host's.
    pub paravirtual_name: bool,
}

/// Whether the present path may run on a device with `caps`, decided before any heap is aliased:
/// `Err` says why not, and frames are then read back. Aliasing another API's placement heap with
/// a texture of our own description is outside what Metal promises; real Apple GPUs keep the
/// image's bytes there, and a VM's paravirtual GPU faults (CI's `macos-26` runner, PR #379).
///
/// A VM's GPU is told apart by the host, not by the device: Metal has no property that marks a
/// virtual GPU, and the families the gate checks don't separate it from a real one. Properties
/// such as `location`, `isRemovable` or the registry class were not checked against one (that
/// needs a VM), so the gate doesn't rely on them; `kern.hv_vmm_present` is the kernel's own flag
/// for running under a hypervisor, and covers a virtual GPU under any name. The name check
/// stays as the fallback for a VM where that flag can't be read.
pub fn present_gate(caps: &Capabilities) -> Result<(), &'static str> {
    if caps.virtual_machine {
        return Err("this Mac is a virtual machine, whose GPU faults when the renderer's heaps are aliased");
    }
    if caps.paravirtual_name {
        return Err("a paravirtual GPU (a macOS VM's): aliasing the renderer's heaps faults there");
    }
    if !caps.placement_heaps {
        return Err("the GPU has no placement heaps, which the renderer's color buffers are");
    }
    if !caps.metal3 {
        return Err("the GPU is not Metal 3");
    }
    Ok(())
}

/// The last post the display was told of: its color buffer and size, noted together when the post
/// is noticed, so a frame is copied only at the size its own buffer was posted at.
#[derive(Default)]
pub struct LastPost(std::sync::Mutex<(u32, u32, u32)>);

impl LastPost {
    /// Color buffer `cb` was posted at `width` x `height`.
    pub fn note(&self, cb: u32, width: u32, height: u32) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (cb, width, height);
    }

    /// `live` (the renderer's last posted buffer now) when it is the noted post and that was
    /// `width` x `height`; `None` for a post not noticed yet (its size isn't known) or one of
    /// another size (a resize landed after the reader took the size).
    pub fn check(&self, live: u32, width: u32, height: u32) -> Option<u32> {
        let noted = *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (live != 0 && noted == (live, width, height)).then_some(live)
    }
}

#[cfg(target_os = "macos")]
pub use imp::{HeapLookup, MachSender, MetalGpu, MetalSurface, capabilities, deallocate_port, system_gate};

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CString, c_char, c_void};

    pub(crate) type Id = *mut c_void;
    pub(crate) type Sel = *const c_void;

    #[link(name = "objc")]
    unsafe extern "C" {
        pub(crate) fn objc_msgSend();
        fn objc_getClass(name: *const c_char) -> Id;
        fn objc_getProtocol(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_retain(obj: Id) -> Id;
        fn objc_release(obj: Id);
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeDictionaryKeyCallBacks: u8;
        static kCFTypeDictionaryValueCallBacks: u8;
        fn CFDictionaryCreateMutable(alloc: *const c_void, capacity: isize, keys: *const u8, values: *const u8) -> *mut c_void;
        fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
        fn CFNumberCreate(alloc: *const c_void, kind: isize, value: *const c_void) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        static kIOSurfaceWidth: *const c_void;
        static kIOSurfaceHeight: *const c_void;
        static kIOSurfaceBytesPerElement: *const c_void;
        static kIOSurfaceBytesPerRow: *const c_void;
        static kIOSurfacePixelFormat: *const c_void;
        fn IOSurfaceCreate(properties: *const c_void) -> *mut c_void;
        fn IOSurfaceCreateMachPort(surface: *mut c_void) -> u32;
        fn IOSurfaceGetID(surface: *mut c_void) -> u32;
        fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
        fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    }

    const MTL_PIXEL_FORMAT_RGBA8: u64 = 70;
    const MTL_PIXEL_FORMAT_BGRA8: u64 = 80;
    const USAGE_SHADER_READ: u64 = 1;
    const USAGE_SHADER_WRITE: u64 = 2;
    const USAGE_RENDER_TARGET: u64 = 4;
    const USAGE_PIXEL_FORMAT_VIEW: u64 = 0x10;
    /// How gfxstream's drivers create color-buffer images (docs/research/metal-present.md): a
    /// texture over the heap with any other usage gets another layout, and reads other bytes.
    pub(crate) const SOURCE_USAGE: u64 = USAGE_SHADER_READ | USAGE_RENDER_TARGET | USAGE_PIXEL_FORMAT_VIEW;
    const STORAGE_SHARED: u64 = 0;
    const HEAP_PLACEMENT: u64 = 1;
    const COMMAND_BUFFER_COMPLETED: u64 = 4;
    const LOCK_READ_ONLY: u32 = 1;

    const KERNEL: &str = "#include <metal_stdlib>\nusing namespace metal;\nkernel void to_bgra(texture2d<float, access::read> src [[texture(0)]], texture2d<float, access::write> dst [[texture(1)]], uint2 gid [[thread_position_in_grid]]) { if (gid.x < dst.get_width() && gid.y < dst.get_height()) dst.write(src.read(gid), gid); }\n";

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct MtlSize {
        pub(crate) width: u64,
        pub(crate) height: u64,
        pub(crate) depth: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct SizeAndAlign {
        pub(crate) size: u64,
        pub(crate) align: u64,
    }

    pub(crate) fn sel(name: &str) -> Sel {
        let c = CString::new(name).unwrap_or_default();
        // SAFETY: a NUL-terminated selector name; the runtime interns it.
        unsafe { sel_registerName(c.as_ptr()) }
    }

    pub(crate) fn class(name: &str) -> Id {
        let c = CString::new(name).unwrap_or_default();
        // SAFETY: a NUL-terminated class name; null when no loaded image defines it.
        unsafe { objc_getClass(c.as_ptr()) }
    }

    pub(crate) fn responds(obj: Id, s: Sel) -> bool {
        if obj.is_null() {
            return false;
        }
        // SAFETY: every NSObject implements `-respondsToSelector:(SEL) -> BOOL`.
        let f = unsafe { std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(Id, Sel, Sel) -> bool>(objc_msgSend) };
        // SAFETY: `obj` is a live object (non-null, from Metal or the runtime).
        unsafe { f(obj, sel("respondsToSelector:"), s) }
    }

    /// `[obj sel args...]` typed as `fn(args...) -> ret`, after `respondsToSelector:` says yes;
    /// `None` when it doesn't (a missing API is a fallback, never a crash).
    macro_rules! send {
        ($obj:expr, $sel:expr $(, $arg:expr => $ty:ty)* ; $ret:ty) => {{
            use $crate::metal::imp::{Id as MId, Sel as MSel};
            let obj: MId = $obj;
            let s = $crate::metal::imp::sel($sel);
            if $crate::metal::imp::responds(obj, s) {
                // SAFETY: the method's signature from the Metal / Foundation headers; arm64
                // `objc_msgSend` takes the receiver, the selector and then the C arguments.
                let f = unsafe {
                    std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(MId, MSel $(, $ty)*) -> $ret>($crate::metal::imp::objc_msgSend)
                };
                // SAFETY: `obj` is live and responds to the selector; arguments match the signature.
                Some::<$ret>(unsafe { f(obj, s $(, $arg)*) })
            } else {
                None
            }
        }};
    }
    // For the tests' own Metal objects.
    #[cfg(test)]
    pub(crate) use send;

    pub(crate) fn conforms(obj: Id, protocol: &str) -> bool {
        let c = CString::new(protocol).unwrap_or_default();
        // SAFETY: a NUL-terminated protocol name; null when no loaded image declares it.
        let p = unsafe { objc_getProtocol(c.as_ptr()) };
        !p.is_null() && send!(obj, "conformsToProtocol:", p => Id; bool).unwrap_or(false)
    }

    /// Releases a +1 object (null is fine).
    pub(crate) fn release(obj: Id) {
        if !obj.is_null() {
            // SAFETY: the caller owns one reference to the live object.
            unsafe { objc_release(obj) }
        }
    }

    /// Runs `f` in an autorelease pool: command buffers and encoders are autoreleased, and the
    /// threads that present have no pool of their own.
    pub(crate) fn pooled<T>(f: impl FnOnce() -> T) -> T {
        // SAFETY: push and pop are balanced on this thread around `f`.
        let pool = unsafe { objc_autoreleasePoolPush() };
        let out = f();
        // SAFETY: the pool pushed above, popped once.
        unsafe { objc_autoreleasePoolPop(pool) };
        out
    }

    #[link(name = "Metal", kind = "framework")]
    unsafe extern "C" {
        fn MTLCreateSystemDefaultDevice() -> Id;
    }

    const FAMILY_APPLE2: i64 = 1002;
    const FAMILY_MAC2: i64 = 2002;
    const FAMILY_METAL3: i64 = 5001;

    /// What `device` says it can do (unknown is false).
    pub fn capabilities(device: *mut c_void) -> super::Capabilities {
        let family = |f: i64| send!(device, "supportsFamily:", f => i64; bool).unwrap_or(false);
        let name = send!(device, "name"; Id)
            .filter(|n| !n.is_null())
            .and_then(|n| send!(n, "UTF8String"; *const c_char))
            .filter(|p| !p.is_null())
            // SAFETY: a NUL-terminated UTF-8 string the NSString owns, read before it can go.
            .map(|p| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
            .unwrap_or_default();
        super::Capabilities {
            metal3: family(FAMILY_METAL3),
            placement_heaps: family(FAMILY_APPLE2) || family(FAMILY_MAC2),
            virtual_machine: hypervisor_present(),
            paravirtual_name: name.contains("Paravirtual"),
        }
    }

    /// Whether this Mac runs under a hypervisor: `kern.hv_vmm_present`, which the kernel sets in a
    /// virtual machine's guest (false when it can't be read).
    pub(crate) fn hypervisor_present() -> bool {
        let mut present: i32 = 0;
        let mut len = std::mem::size_of::<i32>();
        // SAFETY: a NUL-terminated name, an int-sized buffer with its length in `len`, and no new
        // value (null, 0); the kernel writes at most `len` bytes.
        let rc = unsafe {
            libc::sysctlbyname(c"kern.hv_vmm_present".as_ptr(), (&raw mut present).cast(), &raw mut len, std::ptr::null_mut(), 0)
        };
        rc == 0 && present != 0
    }

    /// [`super::present_gate`] for the system's default Metal device, the one the renderer runs on:
    /// asked once when `-gpu metal` sets up, before any frame.
    pub fn system_gate() -> Result<(), String> {
        // SAFETY: returns a +1 device or null without a GPU.
        let device = unsafe { MTLCreateSystemDefaultDevice() };
        if device.is_null() {
            return Err("no Metal device".into());
        }
        let caps = pooled(|| capabilities(device));
        release(device);
        super::present_gate(&caps).map_err(str::to_owned)
    }

    fn nsstring(s: &str) -> Option<Id> {
        let c = CString::new(s).ok()?;
        send!(class("NSString"), "stringWithUTF8String:", c.as_ptr() => *const c_char; Id).filter(|o| !o.is_null())
    }

    /// A texture over a color buffer's heap, retaining both.
    pub struct Source {
        heap: Id,
        texture: Id,
    }

    impl Drop for Source {
        fn drop(&mut self) {
            release(self.texture);
            release(self.heap);
        }
    }

    // SAFETY: Metal resources may be used and released from any thread.
    unsafe impl Send for Source {}

    /// A pooled IOSurface (BGRA) and the Metal texture over it.
    pub struct MetalSurface {
        surface: *mut c_void,
        texture: Id,
        width: u32,
        height: u32,
    }

    // SAFETY: an IOSurface and a Metal texture are thread-safe objects; the GPU writes the surface
    // only while the pool alone holds it (see `Pool`).
    unsafe impl Send for MetalSurface {}
    // SAFETY: as above; shared holders only read it or make ports for it.
    unsafe impl Sync for MetalSurface {}

    impl Drop for MetalSurface {
        fn drop(&mut self) {
            release(self.texture);
            // SAFETY: the +1 surface IOSurfaceCreate gave this handle.
            unsafe { CFRelease(self.surface) };
        }
    }

    fn cf_number(v: i64) -> *const c_void {
        // SAFETY: kCFNumberSInt64Type (4) reads an i64 from a live pointer.
        unsafe { CFNumberCreate(std::ptr::null(), 4, (&raw const v).cast()) }
    }

    impl MetalSurface {
        /// A BGRA IOSurface of `width` x `height` with a shader-writable texture over it on `device`.
        pub(crate) fn new(device: Id, width: u32, height: u32) -> Option<MetalSurface> {
            let align = send!(device, "minimumLinearTextureAlignmentForPixelFormat:", MTL_PIXEL_FORMAT_BGRA8 => u64; u64)?.max(64) as usize;
            let stride = (width as usize * 4).next_multiple_of(align);
            // SAFETY: a CF dictionary with CF-type callbacks; the keys are IOSurface's constant
            // strings, the values fresh numbers the dictionary retains (released after insertion).
            let surface = unsafe {
                let dict = CFDictionaryCreateMutable(
                    std::ptr::null(),
                    0,
                    &raw const kCFTypeDictionaryKeyCallBacks,
                    &raw const kCFTypeDictionaryValueCallBacks,
                );
                for (k, v) in [
                    (kIOSurfaceWidth, i64::from(width)),
                    (kIOSurfaceHeight, i64::from(height)),
                    (kIOSurfaceBytesPerElement, 4),
                    (kIOSurfaceBytesPerRow, stride as i64),
                    (kIOSurfacePixelFormat, i64::from(u32::from_be_bytes(*b"BGRA"))),
                ] {
                    let n = cf_number(v);
                    CFDictionarySetValue(dict, k, n);
                    CFRelease(n);
                }
                let s = IOSurfaceCreate(dict);
                CFRelease(dict);
                s
            };
            if surface.is_null() {
                return None;
            }
            let mut out = MetalSurface { surface, texture: std::ptr::null_mut(), width, height };
            let desc = send!(class("MTLTextureDescriptor"), "texture2DDescriptorWithPixelFormat:width:height:mipmapped:",
                MTL_PIXEL_FORMAT_BGRA8 => u64, u64::from(width) => u64, u64::from(height) => u64, false => bool; Id)?;
            send!(desc, "setStorageMode:", STORAGE_SHARED => u64; ())?;
            send!(desc, "setUsage:", USAGE_SHADER_READ | USAGE_SHADER_WRITE => u64; ())?;
            out.texture = send!(device, "newTextureWithDescriptor:iosurface:plane:", desc => Id, surface => *mut c_void, 0 => u64; Id)?;
            (!out.texture.is_null()).then_some(out)
        }

        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        /// The `IOSurfaceRef`, valid while this handle lives.
        pub fn as_raw(&self) -> *mut c_void {
            self.surface
        }

        /// Its system-wide ID (the same in every process that has it).
        pub fn id(&self) -> u32 {
            // SAFETY: a live surface.
            unsafe { IOSurfaceGetID(self.surface) }
        }

        /// A new send right for the surface (`IOSurfaceCreateMachPort`), owned by the caller;
        /// `None` if none could be made.
        pub fn mach_port(&self) -> Option<u32> {
            // SAFETY: a live surface; the result is a new right (0 on failure).
            Some(unsafe { IOSurfaceCreateMachPort(self.surface) }).filter(|&p| p != 0)
        }

        /// Runs `f` on the surface's BGRA rows (and their stride) with it locked for reading.
        pub fn with_bgra<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> Option<R> {
            // SAFETY: a live surface, locked read-only and unlocked below; while locked its
            // `height` rows of `stride` bytes are mapped at the base address, and the slice
            // doesn't outlive the lock.
            unsafe {
                if IOSurfaceLock(self.surface, LOCK_READ_ONLY, std::ptr::null_mut()) != 0 {
                    return None;
                }
                let base = IOSurfaceGetBaseAddress(self.surface).cast::<u8>();
                let stride = IOSurfaceGetBytesPerRow(self.surface);
                let out = (!base.is_null()).then(|| f(std::slice::from_raw_parts(base, stride * self.height as usize), stride));
                IOSurfaceUnlock(self.surface, LOCK_READ_ONLY, std::ptr::null_mut());
                out
            }
        }
    }

    /// The Metal device, queue and pipeline, made from the first heap's device.
    struct Device {
        device: Id,
        queue: Id,
        pipeline: Id,
        /// Threads per threadgroup (width, height) the pipeline takes.
        group: (u64, u64),
    }

    impl Drop for Device {
        fn drop(&mut self) {
            release(self.pipeline);
            release(self.queue);
            release(self.device);
        }
    }

    impl Device {
        fn new(device: Id) -> Option<Device> {
            // SAFETY: a live device; retained for this handle, released by its Drop.
            let device = unsafe { objc_retain(device) };
            let mut d = Device { device, queue: std::ptr::null_mut(), pipeline: std::ptr::null_mut(), group: (8, 8) };
            d.queue = send!(device, "newCommandQueue"; Id)?;
            let mut err: Id = std::ptr::null_mut();
            let lib = pooled(
                || send!(device, "newLibraryWithSource:options:error:", nsstring(KERNEL)? => Id, std::ptr::null_mut() => Id, &raw mut err => *mut Id; Id),
            )?;
            if lib.is_null() {
                return None;
            }
            let f = pooled(|| send!(lib, "newFunctionWithName:", nsstring("to_bgra")? => Id; Id));
            release(lib);
            let f = f.filter(|f| !f.is_null())?;
            d.pipeline = send!(device, "newComputePipelineStateWithFunction:error:", f => Id, &raw mut err => *mut Id; Id)
                .unwrap_or(std::ptr::null_mut());
            release(f);
            if d.queue.is_null() || d.pipeline.is_null() {
                return None;
            }
            // A SIMD group wide, as many rows as the pipeline allows.
            let width = send!(d.pipeline, "threadExecutionWidth"; u64).unwrap_or(8).max(1);
            let max = send!(d.pipeline, "maxTotalThreadsPerThreadgroup"; u64).unwrap_or(64);
            d.group = (width.min(max).max(1), (max / width).clamp(1, 16));
            Some(d)
        }
    }

    /// Looks a color buffer's heap up (unretained, null without one).
    pub type HeapLookup = Box<dyn FnMut(u32) -> *mut c_void + Send>;

    /// [`super::Gpu`] on Metal: sources alias the renderer's placement heaps, targets are
    /// IOSurfaces, the copy is one compute pass, waited for.
    pub struct MetalGpu {
        lookup: HeapLookup,
        device: Option<Device>,
        /// Whether the heaps' device is put to [`super::present_gate`] first (tests that probe a
        /// refused device turn it off).
        gated: bool,
        /// The device was refused: nothing is aliased, every present is `None`.
        refused: bool,
        /// Copies that completed, for the counters a run reports.
        pub copies: u64,
    }

    // SAFETY: the Metal objects (device, queue, pipeline) are thread-safe; the lookup is Send.
    unsafe impl Send for MetalGpu {}

    impl MetalGpu {
        pub fn new(lookup: HeapLookup) -> MetalGpu {
            MetalGpu { lookup, device: None, gated: true, refused: false, copies: 0 }
        }

        /// A [`MetalGpu`] that skips [`super::present_gate`], for a test that finds out where a
        /// refused device fails.
        #[cfg(test)]
        pub(crate) fn ungated(lookup: HeapLookup) -> MetalGpu {
            MetalGpu { gated: false, ..MetalGpu::new(lookup) }
        }

        /// Whether the queue and pipeline set up on `heap`'s device, for that test.
        #[cfg(test)]
        pub(crate) fn sets_up_on(&mut self, heap: *mut c_void) -> bool {
            self.device_for(heap).is_some()
        }

        /// The device the heaps live on, set up once, if [`super::present_gate`] lets it run the
        /// copy: decided before any heap is aliased.
        fn device_for(&mut self, heap: Id) -> Option<&Device> {
            if self.device.is_none() {
                if self.refused {
                    return None;
                }
                let device = send!(heap, "device"; Id).filter(|d| !d.is_null())?;
                if self.gated
                    && let Err(why) = super::present_gate(&pooled(|| capabilities(device)))
                {
                    eprintln!("emu: -gpu metal: {why}; frames are read back");
                    self.refused = true;
                    return None;
                }
                self.device = Some(Device::new(device)?);
            }
            self.device.as_ref()
        }
    }

    impl super::Gpu for MetalGpu {
        type Source = Source;
        type Target = MetalSurface;

        fn memory(&mut self, cb: u32) -> usize {
            (self.lookup)(cb) as usize
        }

        fn source(&mut self, memory: usize, width: u32, height: u32) -> Option<Source> {
            let heap = memory as Id;
            if !conforms(heap, "MTLHeap") || send!(heap, "type"; u64)? != HEAP_PLACEMENT {
                return None;
            }
            // Same device for every heap of the renderer.
            self.device_for(heap)?;
            // SAFETY: a live heap (its color buffer is the posted one); retained by the source.
            let heap = unsafe { objc_retain(heap) };
            let mut source = Source { heap, texture: std::ptr::null_mut() };
            let texture = pooled(|| {
                let desc = send!(class("MTLTextureDescriptor"), "texture2DDescriptorWithPixelFormat:width:height:mipmapped:",
                    MTL_PIXEL_FORMAT_RGBA8 => u64, u64::from(width) => u64, u64::from(height) => u64, false => bool; Id)?;
                // As the heap's memory is: its storage, caching and hazard tracking.
                let storage = send!(heap, "storageMode"; u64)?;
                let cache = send!(heap, "cpuCacheMode"; u64)?;
                let hazard = send!(heap, "hazardTrackingMode"; u64)?;
                send!(desc, "setStorageMode:", storage => u64; ())?;
                send!(desc, "setCpuCacheMode:", cache => u64; ())?;
                send!(desc, "setHazardTrackingMode:", hazard => u64; ())?;
                send!(desc, "setUsage:", SOURCE_USAGE => u64; ())?;
                let device = send!(heap, "device"; Id)?;
                let need = send!(device, "heapTextureSizeAndAlignWithDescriptor:", desc => Id; SizeAndAlign)?;
                if need.size > send!(heap, "size"; u64)? {
                    return None;
                }
                send!(heap, "newTextureWithDescriptor:offset:", desc => Id, 0 => u64; Id)
            })?;
            source.texture = texture;
            (!texture.is_null()).then_some(source)
        }

        fn target(&mut self, width: u32, height: u32) -> Option<MetalSurface> {
            MetalSurface::new(self.device.as_ref()?.device, width, height)
        }

        fn copy(&mut self, src: &Source, dst: &MetalSurface, width: u32, height: u32) -> bool {
            let Some(d) = self.device.as_ref() else { return false };
            let done = pooled(|| {
                let cmd = send!(d.queue, "commandBuffer"; Id).filter(|c| !c.is_null())?;
                let enc = send!(cmd, "computeCommandEncoder"; Id).filter(|e| !e.is_null())?;
                send!(enc, "setComputePipelineState:", d.pipeline => Id; ())?;
                send!(enc, "setTexture:atIndex:", src.texture => Id, 0 => u64; ())?;
                send!(enc, "setTexture:atIndex:", dst.texture => Id, 1 => u64; ())?;
                // Whole threadgroups covering the image (the kernel skips threads past its edge):
                // non-uniform threadgroups are a GPU family feature this needn't ask for.
                let (gw, gh) = d.group;
                let groups = MtlSize { width: u64::from(width).div_ceil(gw), height: u64::from(height).div_ceil(gh), depth: 1 };
                let group = MtlSize { width: gw, height: gh, depth: 1 };
                send!(enc, "dispatchThreadgroups:threadsPerThreadgroup:", groups => MtlSize, group => MtlSize; ())?;
                send!(enc, "endEncoding"; ())?;
                send!(cmd, "commit"; ())?;
                send!(cmd, "waitUntilCompleted"; ())?;
                send!(cmd, "status"; u64)
            }) == Some(COMMAND_BUFFER_COMPLETED);
            self.copies += u64::from(done);
            done
        }
    }

    // --- Mach: the surface's send right to a client's port (`sim_core::stream::Record::Surface`).
    // The receiver is `sim_coresim::SurfaceInbox`, with the same message layout.

    type MachPort = u32;

    const MACH_MSG_TYPE_MOVE_SEND: u8 = 17;
    const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
    const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
    const MACH_SEND_MSG: i32 = 1;
    const MACH_SEND_TIMEOUT: i32 = 0x10;

    #[repr(C)]
    struct Message {
        bits: u32,
        size: u32,
        remote: MachPort,
        local: MachPort,
        voucher: MachPort,
        id: i32,
        descriptors: u32,
        port: MachPort,
        pad1: u32,
        pad2: u16,
        disposition: u8,
        kind: u8,
        seq: u32,
    }

    unsafe extern "C" {
        static bootstrap_port: MachPort;
        static mach_task_self_: MachPort;
        fn bootstrap_look_up(bp: MachPort, name: *const c_char, port: *mut MachPort) -> i32;
        fn mach_msg(
            msg: *mut c_void,
            option: i32,
            send_size: u32,
            rcv_size: u32,
            rcv_name: MachPort,
            timeout: u32,
            notify: MachPort,
        ) -> i32;
        fn mach_port_deallocate(task: MachPort, name: MachPort) -> i32;
    }

    /// Frees a send right this task holds (one [`MetalSurface::mach_port`] made and didn't send).
    pub fn deallocate_port(port: u32) {
        // SAFETY: a right this task holds, freed once.
        unsafe { mach_port_deallocate(mach_task_self_, port) };
    }

    /// A send right to a client's surface port, looked up by the bootstrap name it gave
    /// (`surfaces <name>`).
    pub struct MachSender {
        port: MachPort,
    }

    // SAFETY: a send right's name is valid on every thread of the task; each send is atomic.
    unsafe impl Send for MachSender {}
    // SAFETY: as above.
    unsafe impl Sync for MachSender {}

    impl MachSender {
        pub fn look_up(name: &str) -> Option<MachSender> {
            let c = CString::new(name).ok()?;
            let mut port = 0;
            // SAFETY: the task's bootstrap port, a NUL-terminated name and a live out-pointer.
            let kr = unsafe { bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut port) };
            (kr == 0 && port != 0).then_some(MachSender { port })
        }

        /// The send right's name, for tests that count its references.
        #[cfg(test)]
        pub(crate) fn port(&self) -> u32 {
            self.port
        }

        /// Moves send right `surface` to the client as surface `seq`, without blocking: false
        /// (and the right freed) when it can't go now (the client's queue is full, or it's gone).
        pub fn send(&self, surface: u32, seq: u32) -> bool {
            let mut m = Message {
                bits: MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX,
                size: std::mem::size_of::<Message>() as u32,
                remote: self.port,
                local: 0,
                voucher: 0,
                id: sim_core::stream::SURFACE_MSG_ID,
                descriptors: 1,
                port: surface,
                pad1: 0,
                pad2: 0,
                disposition: MACH_MSG_TYPE_MOVE_SEND,
                kind: 0,
                seq,
            };
            // SAFETY: a well-formed complex message of the size given, to our send right.
            let kr = unsafe { mach_msg((&raw mut m).cast(), MACH_SEND_MSG | MACH_SEND_TIMEOUT, m.size, 0, 0, 0, 0) };
            if kr != 0 {
                // Not sent: the right is still ours (returned under its name), so free it.
                // SAFETY: a right this task holds.
                unsafe { mach_port_deallocate(mach_task_self_, m.port) };
                // Copied in and then handed back (a full queue times out): the destination came
                // back too, one more reference on our right to the client, dropped here.
                if sim_core::stream::mach_send_pseudo_received(kr) && m.remote != 0 {
                    // SAFETY: the reference the pseudo-receive just gave this task.
                    unsafe { mach_port_deallocate(mach_task_self_, m.remote) };
                }
            }
            kr == 0
        }
    }

    impl Drop for MachSender {
        fn drop(&mut self) {
            // SAFETY: our send right, released once.
            unsafe { mach_port_deallocate(mach_task_self_, self.port) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Records what the presenter asked of it, in order.
    #[derive(Default)]
    struct Fake {
        /// Each color buffer's memory (an address; 0: none).
        memory: HashMap<u32, usize>,
        calls: Vec<String>,
        /// Sources alive (made and not dropped).
        live: Arc<std::sync::atomic::AtomicUsize>,
        fail_copy: bool,
        made: usize,
    }

    struct FakeSource {
        memory: usize,
        live: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Drop for FakeSource {
        fn drop(&mut self) {
            self.live.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[derive(Debug)]
    struct FakeTarget {
        n: usize,
        /// The memory last copied in.
        from: std::sync::Mutex<usize>,
    }

    impl Gpu for Fake {
        type Source = FakeSource;
        type Target = FakeTarget;
        fn memory(&mut self, cb: u32) -> usize {
            self.calls.push(format!("memory {cb}"));
            self.memory.get(&cb).copied().unwrap_or(0)
        }
        fn source(&mut self, memory: usize, w: u32, h: u32) -> Option<FakeSource> {
            self.calls.push(format!("source {memory:#x} {w}x{h}"));
            self.live.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(FakeSource { memory, live: self.live.clone() })
        }
        fn target(&mut self, w: u32, h: u32) -> Option<FakeTarget> {
            self.calls.push(format!("target {w}x{h}"));
            self.made += 1;
            Some(FakeTarget { n: self.made, from: std::sync::Mutex::new(0) })
        }
        fn copy(&mut self, src: &FakeSource, dst: &FakeTarget, _: u32, _: u32) -> bool {
            self.calls.push(format!("copy {:#x} -> {}", src.memory, dst.n));
            *dst.from.lock().expect("lock") = src.memory;
            !self.fail_copy
        }
    }

    fn fake(buffers: &[(u32, usize)]) -> Presenter<Fake> {
        Presenter::new(Fake { memory: buffers.iter().copied().collect(), ..Fake::default() })
    }

    #[test]
    fn the_pool_reuses_a_surface_only_once_every_holder_released_it() {
        let mut pool = Pool::<u32>::default();
        let mut n = 0;
        let mut make = || {
            n += 1;
            Some(n)
        };
        let a = pool.acquire((4, 4), &mut make).expect("a");
        let b = pool.acquire((4, 4), &mut make).expect("b");
        let c = pool.acquire((4, 4), &mut make).expect("c");
        assert_eq!((*a, *b, *c), (1, 2, 3));
        assert!(pool.acquire((4, 4), &mut make).is_none(), "all three held: nothing is written over");
        // A consumer's lease (a client's, until `release`) holds it as much as the first holder.
        let lease = b.clone();
        drop(b);
        assert!(pool.acquire((4, 4), &mut make).is_none(), "still leased");
        drop(lease);
        let again = pool.acquire((4, 4), &mut make).expect("released");
        assert_eq!(*again, 2, "the released one, not a new one");
        assert_eq!(pool.len(), POOL);
        drop((a, c, again));
        // A new size starts over; surfaces of the old size held elsewhere live on.
        let old = pool.acquire((4, 4), &mut make).expect("old size");
        let new = pool.acquire((8, 8), &mut make).expect("new size");
        assert_eq!((*old, *new, pool.len()), (1, 4, 1));
    }

    #[test]
    fn a_present_looks_the_memory_up_aliases_it_once_and_copies_into_a_free_surface() {
        let mut p = fake(&[(1, 0x1000), (2, 0x2000), (3, 0x3000)]);
        // Host composition cycles through three targets.
        let mut held = Vec::new();
        for cb in [1, 2, 3] {
            held.push(p.present(cb, 4, 2).expect("present"));
        }
        assert_eq!(
            p.gpu.calls,
            [
                "memory 1",
                "source 0x1000 4x2",
                "target 4x2",
                "copy 0x1000 -> 1",
                "memory 2",
                "source 0x2000 4x2",
                "target 4x2",
                "copy 0x2000 -> 2",
                "memory 3",
                "source 0x3000 4x2",
                "target 4x2",
                "copy 0x3000 -> 3",
            ]
        );
        // Every surface held: the next post has none to go into, and nothing is copied.
        p.gpu.calls.clear();
        assert!(p.present(1, 4, 2).is_none());
        assert_eq!(p.gpu.calls, ["memory 1"], "no copy over a held surface");
        // The first released: it takes the next frame, from the cached source.
        drop(held.remove(0));
        p.gpu.calls.clear();
        let s = p.present(1, 4, 2).expect("present");
        assert_eq!(p.gpu.calls, ["memory 1", "copy 0x1000 -> 1"]);
        assert_eq!((s.n, *s.from.lock().expect("lock")), (1, 0x1000));
        assert_eq!(p.sources(), 3);
    }

    #[test]
    fn a_handle_whose_memory_changed_is_aliased_again_and_the_old_memory_let_go() {
        let mut p = fake(&[(1, 0x1000)]);
        drop(p.present(1, 4, 2).expect("present"));
        assert_eq!(p.gpu.live.load(std::sync::atomic::Ordering::Relaxed), 1);
        // The buffer was freed and its handle reused for a new one: new memory.
        p.gpu.memory.insert(1, 0x5000);
        p.gpu.calls.clear();
        drop(p.present(1, 4, 2).expect("present"));
        assert_eq!(p.gpu.calls, ["memory 1", "source 0x5000 4x2", "copy 0x5000 -> 1"]);
        assert_eq!((p.sources(), p.gpu.live.load(std::sync::atomic::Ordering::Relaxed)), (1, 1), "the old source (and its retain) went");
        // A new size is a new source too.
        p.gpu.calls.clear();
        drop(p.present(1, 8, 4).expect("present"));
        assert_eq!(p.gpu.calls, ["memory 1", "source 0x5000 8x4", "target 8x4", "copy 0x5000 -> 2"]);
    }

    #[test]
    fn sources_are_bounded_and_the_least_recently_used_goes() {
        let buffers: Vec<(u32, usize)> = (1..=SOURCES as u32 + 2).map(|cb| (cb, cb as usize * 0x1000)).collect();
        let mut p = fake(&buffers);
        for &(cb, _) in &buffers {
            drop(p.present(cb, 2, 2).expect("present"));
        }
        assert_eq!((p.sources(), p.gpu.live.load(std::sync::atomic::Ordering::Relaxed)), (SOURCES, SOURCES));
        // The first two went; the third is still cached.
        p.gpu.calls.clear();
        drop(p.present(3, 2, 2));
        drop(p.present(1, 2, 2));
        assert_eq!(p.gpu.calls, ["memory 3", "copy 0x3000 -> 1", "memory 1", "source 0x1000 2x2", "copy 0x1000 -> 1"]);
    }

    #[test]
    fn nothing_is_handed_out_without_memory_or_after_a_failed_copy() {
        let mut p = fake(&[(1, 0x1000)]);
        assert!(p.present(9, 4, 2).is_none(), "no memory for that buffer");
        assert_eq!(p.gpu.calls, ["memory 9"]);
        p.gpu.fail_copy = true;
        assert!(p.present(1, 4, 2).is_none(), "the copy failed");
        // Its surface went back to the pool: the next copy uses it.
        p.gpu.fail_copy = false;
        let s = p.present(1, 4, 2).expect("present");
        assert_eq!((s.n, p.pool().len()), (1, 1));
    }

    #[test]
    fn surfaces_are_checked_against_the_readback_with_red_and_blue_swapped() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8];
        let bgra = [3, 2, 1, 4, 7, 6, 5, 8, 0, 0, 0, 0];
        assert!(bgra_matches_rgba(&bgra, 12, &rgba, 2, 1));
        assert!(!bgra_matches_rgba(&rgba, 8, &rgba, 2, 1), "unswapped");
        let two_rows = [3, 2, 1, 4, 9, 9, 7, 6, 5, 8, 9, 9];
        assert!(bgra_matches_rgba(&two_rows, 6, &rgba, 1, 2), "rows a stride apart");
        assert!(!bgra_matches_rgba(&bgra[..4], 12, &rgba, 2, 1), "short");
    }

    #[test]
    fn the_present_path_is_refused_up_front_on_a_gpu_that_cant_alias_heaps() {
        let apple = Capabilities { metal3: true, placement_heaps: true, virtual_machine: false, paravirtual_name: false };
        assert_eq!(present_gate(&apple), Ok(()));
        let why = |caps| present_gate(&caps).expect_err("refused");
        assert!(why(Capabilities { paravirtual_name: true, ..apple }).contains("virtual"), "a VM's GPU, by its name");
        assert!(why(Capabilities { placement_heaps: false, ..apple }).contains("placement heaps"));
        assert!(why(Capabilities { metal3: false, ..apple }).contains("Metal 3"));
        assert!(present_gate(&Capabilities::default()).is_err(), "nothing known: refused");
    }

    #[test]
    fn a_virtual_machines_gpu_is_refused_whatever_it_is_called() {
        // A VM's GPU that reports everything a real one does and isn't named "Paravirtual".
        let renamed = Capabilities { metal3: true, placement_heaps: true, virtual_machine: true, paravirtual_name: false };
        assert!(present_gate(&renamed).expect_err("refused").contains("virtual machine"));
        let named = Capabilities { paravirtual_name: true, ..renamed };
        assert!(present_gate(&named).is_err(), "both signals: refused");
    }

    #[test]
    fn a_frame_is_copied_only_at_the_size_its_buffer_was_posted_at() {
        let post = LastPost::default();
        assert_eq!(post.check(0, 64, 32), None, "nothing posted");
        post.note(7, 64, 32);
        assert_eq!(post.check(7, 64, 32), Some(7));
        // A resize: the renderer names buffer 8 before the display hears of the post.
        assert_eq!(post.check(8, 64, 32), None, "a post not noticed yet: its size isn't known");
        post.note(8, 128, 64);
        assert_eq!(post.check(8, 64, 32), None, "the reader took the old post's size");
        assert_eq!(post.check(8, 128, 64), Some(8));
    }

    /// The Mach side, which needs no GPU.
    #[cfg(target_os = "macos")]
    mod on_mach {
        use super::super::*;

        unsafe extern "C" {
            static mach_task_self_: u32;
            fn mach_port_get_refs(task: u32, name: u32, right: u32, refs: *mut u32) -> i32;
        }

        /// User references this task holds on send right `port`.
        pub(crate) fn send_refs(port: u32) -> u32 {
            let mut refs = 0;
            // SAFETY: the task's own port, a name it holds and a live out-pointer;
            // MACH_PORT_RIGHT_SEND is 0.
            let kr = unsafe { mach_port_get_refs(mach_task_self_, port, 0, &mut refs) };
            assert_eq!(kr, 0, "mach_port_get_refs");
            refs
        }

        #[test]
        fn a_refused_send_leaves_the_senders_right_to_the_client_as_it_was() {
            let inbox = sim_coresim::SurfaceInbox::check_in(std::process::id() as i32).expect("check in");
            let sender = MachSender::look_up(inbox.name()).expect("look up");
            let surface = sim_coresim::Surface::new_bgra(4, 4).expect("surface");
            let before = send_refs(sender.port());
            // Nobody receives: the client's queue fills, and the sends after that time out.
            let sent = (0..64).take_while(|&i| sender.send(surface.mach_port().expect("port"), i)).count();
            assert!(sent < 64, "a port queues a few messages, not 64");
            for i in 0..3 {
                assert!(!sender.send(surface.mach_port().expect("port"), 100 + i), "still full");
            }
            assert_eq!(send_refs(sender.port()), before, "a timed-out send hands the destination back: dropped again");
        }
    }

    #[cfg(target_os = "macos")]
    mod on_metal {
        use super::super::imp::{MtlSize, SOURCE_USAGE, SizeAndAlign, capabilities, class, pooled, release, send};
        use super::super::*;
        use std::ffi::c_void;
        use std::time::{Duration, Instant};

        type Id = *mut c_void;

        #[link(name = "Metal", kind = "framework")]
        unsafe extern "C" {
            fn MTLCreateSystemDefaultDevice() -> Id;
        }

        /// Set in the child that runs the copy test with the gate off ([`probe_with_the_gate_off`]).
        const PROBE: &str = "SIMEE_METAL_PROBE";
        const COPY_TEST: &str = "metal::tests::on_metal::a_color_buffer_heap_is_copied_into_a_bgra_surface_byte_for_byte";

        /// How long the probe may run before it stops itself, and how long the test waits for it:
        /// both inside the 30 s the default nextest profile gives a test, and the child's bound on
        /// its own, so a parent killed while it waits leaves no hung child behind.
        const PROBE_SELF_BOUND: Duration = Duration::from_secs(15);
        const PROBE_WAIT: Duration = Duration::from_secs(20);
        /// What the probe prints when it stops itself (not a step, so the last step stays).
        const PROBE_GAVE_UP: &str = "metal probe hung and stopped itself";

        fn probing() -> bool {
            std::env::var_os(PROBE).is_some()
        }

        /// In the probe, names the step about to run on stderr: a fault leaves the last one.
        fn step(what: &str) {
            if probing() {
                eprintln!("metal probe: {what}");
            }
        }

        #[repr(C)]
        struct Region {
            origin: MtlSize,
            size: MtlSize,
        }

        /// A placement heap holding one RGBA texture of `w` x `h` made as gfxstream's drivers make
        /// color buffers, filled with `rgba`: the heap (+1), or None without a GPU that has them.
        fn color_buffer_heap(device: Id, w: u32, h: u32, rgba: &[u8]) -> Option<Id> {
            pooled(|| {
                let desc = send!(class("MTLTextureDescriptor"), "texture2DDescriptorWithPixelFormat:width:height:mipmapped:",
                    70u64 => u64, u64::from(w) => u64, u64::from(h) => u64, false => bool; Id)?;
                send!(desc, "setStorageMode:", 0u64 => u64; ())?;
                send!(desc, "setUsage:", SOURCE_USAGE => u64; ())?;
                step("heapTextureSizeAndAlignWithDescriptor:");
                let need = send!(device, "heapTextureSizeAndAlignWithDescriptor:", desc => Id; SizeAndAlign)?;
                let hd = send!(send!(class("MTLHeapDescriptor"), "alloc"; Id)?, "init"; Id)?;
                send!(hd, "setType:", 1u64 => u64; ())?;
                send!(hd, "setStorageMode:", 0u64 => u64; ())?;
                send!(hd, "setSize:", need.size => u64; ())?;
                step("newHeapWithDescriptor: (placement, shared)");
                let heap = send!(device, "newHeapWithDescriptor:", hd => Id; Id).filter(|h| !h.is_null());
                release(hd);
                let heap = heap?;
                step("heap newTextureWithDescriptor:offset:");
                let t = send!(heap, "newTextureWithDescriptor:offset:", desc => Id, 0u64 => u64; Id).filter(|t| !t.is_null())?;
                let region = Region {
                    origin: MtlSize { width: 0, height: 0, depth: 0 },
                    size: MtlSize { width: u64::from(w), height: u64::from(h), depth: 1 },
                };
                step("replaceRegion: into the heap's texture");
                send!(t, "replaceRegion:mipmapLevel:withBytes:bytesPerRow:", region => Region, 0u64 => u64, rgba.as_ptr() => *const u8, u64::from(w) * 4 => u64; ())?;
                // The heap keeps the memory; the texture that wrote it can go.
                release(t);
                Some(heap)
            })
        }

        /// Runs the copy test again in a child process with the gate off and says how far it got:
        /// where a GPU the gate refuses fails, without a fault taking this test binary down.
        fn probe_with_the_gate_off() -> String {
            use std::io::Read;
            let Ok(exe) = std::env::current_exe() else { return "no test binary to probe with".into() };
            let spawned = std::process::Command::new(exe)
                .args([COPY_TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(PROBE, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn();
            let Ok(mut child) = spawned else { return "the probe didn't start".into() };
            let mut stderr = child.stderr.take();
            let reader = std::thread::spawn(move || {
                let mut out = String::new();
                if let Some(e) = stderr.as_mut() {
                    let _ = e.read_to_string(&mut out);
                }
                out
            });
            // Only a bound on a hung probe (a copy never completing); nothing is timed.
            let deadline = Instant::now() + PROBE_WAIT;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(s)) => break s.to_string(),
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break "hung, stopped".into();
                    }
                }
            };
            let out = reader.join().unwrap_or_default();
            let status = if out.contains(PROBE_GAVE_UP) { "hung, stopped".into() } else { status };
            let last = out.lines().rev().find_map(|l| l.strip_prefix("metal probe: ")).unwrap_or("none").to_owned();
            format!("with the gate off the copy {status}; the last step it reached: {last}")
        }

        #[test]
        fn a_color_buffer_heap_is_copied_into_a_bgra_surface_byte_for_byte() {
            if probing() {
                // A copy that never completes must not outlive the test that started the probe.
                std::thread::spawn(|| {
                    std::thread::sleep(PROBE_SELF_BOUND);
                    eprintln!("{PROBE_GAVE_UP}");
                    std::process::abort();
                });
            }
            // SAFETY: returns a +1 device or null without a GPU.
            let device = unsafe { MTLCreateSystemDefaultDevice() };
            if device.is_null() {
                eprintln!("no Metal device: skipped");
                return;
            }
            // A GPU emu won't run the copy on is skipped, saying why, and where it fails.
            if !probing()
                && let Err(why) = present_gate(&pooled(|| capabilities(device)))
            {
                eprintln!("skipped: emu refuses the present path on this GPU ({why})");
                eprintln!("{}", probe_with_the_gate_off());
                release(device);
                return;
            }
            let (w, h) = (67, 35);
            let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
            let Some(heap) = color_buffer_heap(device, w, h, &rgba) else {
                eprintln!("no placement heaps on this GPU: skipped");
                release(device);
                return;
            };
            let addr = heap as usize;
            let lookup = move |cb| if cb == 5 { addr as *mut c_void } else { std::ptr::null_mut() };
            if probing() {
                // Each of the present's steps on its own, so the last one named is the one that
                // failed.
                let mut gpu = MetalGpu::ungated(Box::new(lookup));
                step("the device's queue and pipeline");
                assert!(gpu.sets_up_on(heap), "device");
                step("a texture over the heap (source)");
                let src = gpu.source(addr, w, h).expect("source");
                step("an IOSurface and a texture over it (target)");
                let dst = gpu.target(w, h).expect("target");
                step("the compute pass, waited for (copy)");
                assert!(gpu.copy(&src, &dst, w, h), "copy");
                step("the surface read back");
                assert_eq!(dst.with_bgra(|px, stride| bgra_matches_rgba(px, stride, &rgba, w, h)), Some(true));
                step("done");
                return;
            }
            let mut p = Presenter::new(MetalGpu::new(Box::new(lookup)));
            assert!(p.present(6, w, h).is_none(), "no heap for that buffer");
            let s = p.present(5, w, h).expect("copied");
            assert_eq!(s.size(), (w, h));
            assert_eq!(
                s.with_bgra(|px, stride| bgra_matches_rgba(px, stride, &rgba, w, h)),
                Some(true),
                "the image's bytes, red and blue swapped"
            );
            deallocate_port(s.mach_port().expect("a port for it"));
            // The source holds the heap: the presenter's copy outlives the test's reference.
            release(heap);
            drop(s);
            let again = p.present(5, w, h).expect("copied again");
            assert_eq!(again.with_bgra(|px, stride| bgra_matches_rgba(px, stride, &rgba, w, h)), Some(true));
            release(device);
        }

        /// `kern.hv_vmm_present` as `sysctl` prints it: an independent read of the flag.
        fn sysctl_says_virtual_machine() -> bool {
            let out = std::process::Command::new("/usr/sbin/sysctl").args(["-n", "kern.hv_vmm_present"]).output().expect("sysctl");
            String::from_utf8_lossy(&out.stdout).trim() == "1"
        }

        #[test]
        fn this_macs_gpu_is_allowed_unless_this_mac_is_a_virtual_machine() {
            // SAFETY: returns a +1 device or null without a GPU.
            let device = unsafe { MTLCreateSystemDefaultDevice() };
            if device.is_null() {
                eprintln!("no Metal device: skipped");
                return;
            }
            let caps = pooled(|| capabilities(device));
            release(device);
            let vm = sysctl_says_virtual_machine();
            assert_eq!(caps.virtual_machine, vm, "{caps:?}");
            if vm {
                assert!(present_gate(&caps).is_err(), "a VM's GPU: {caps:?}");
            } else {
                // Outside a VM, the Macs this runs on have Metal 3 GPUs with placement heaps.
                assert_eq!(present_gate(&caps), Ok(()), "{caps:?}");
            }
        }

        #[test]
        fn a_surface_reaches_a_client_port_through_the_bootstrap_name_it_gave() {
            // SAFETY: returns a +1 device or null without a GPU.
            let device = unsafe { MTLCreateSystemDefaultDevice() };
            if device.is_null() {
                eprintln!("no Metal device: skipped");
                return;
            }
            let surface = MetalSurface::new(device, 64, 32).expect("surface");
            // The client's side is simee's (`sim_coresim::SurfaceInbox`): the wire format is
            // shared, not the code.
            let inbox = sim_coresim::SurfaceInbox::check_in(std::process::id() as i32).expect("check in");
            let sender = MachSender::look_up(inbox.name()).expect("look up");
            assert!(MachSender::look_up("dev.simee.surfaces.nobody").is_none());
            assert!(sender.send(surface.mach_port().expect("port"), 41));
            let (seq, got) = inbox.recv(std::time::Duration::from_secs(1)).expect("recv").expect("a surface");
            assert_eq!((seq, got.id(), got.size()), (41, surface.id(), (64, 32)), "the same surface, by its system-wide ID");
            // A client gone: the send fails at once and the right is freed, never queued.
            drop(inbox);
            assert!(!sender.send(surface.mach_port().expect("port"), 42));
            release(device);
        }
    }
}
