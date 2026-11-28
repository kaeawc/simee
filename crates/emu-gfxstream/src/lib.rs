//! Runtime binding to gfxstream's `stream_renderer_*` C API (virtio-gpu 3D on the host).
//!
//! The Android SDK ships the renderer as `emulator/lib64/libgfxstream_backend.dylib` (macOS) or
//! `.so` (Linux x86_64); it is `dlopen`ed here so simee needs no build-time dependency on it. The
//! renderer is process-wide state, so there is at most one [`Renderer`] per process. Signatures
//! follow `docs/reference/gfxstream/virtio-gpu-gfxstream-renderer.h`.
#![cfg(any(target_os = "macos", target_os = "linux"))]
// `sym!` transmutes dlsym results to the type of the struct field it initializes.
#![allow(clippy::missing_transmute_annotations)]

// The opengles pipe's channel reads the renderer's Mach-O code to size its objects.
#[cfg(target_os = "macos")]
mod channel;
#[cfg(target_os = "linux")]
mod elf;
#[cfg(target_os = "macos")]
mod macho;
pub mod metal;

#[cfg(target_os = "macos")]
pub use channel::{ChannelRead, ChannelWrite, RenderChannel};

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

pub use emu_hvf::HostRange;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("loading {0}: {1}")]
    Load(String, String),
    #[error("missing symbol {0}")]
    Symbol(&'static str),
    #[error("{0} failed ({1})")]
    Call(&'static str, i32),
    #[error("the gfxstream renderer is already initialized in this process")]
    AlreadyInitialized,
    #[error("unavailable in this renderer: {0}")]
    Unavailable(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Capability sets gfxstream exposes (virtio-gpu capset ids).
pub mod capset {
    pub const GFXSTREAM_VULKAN: u32 = 3;
    pub const GFXSTREAM_GLES: u32 = 8;
    pub const GFXSTREAM_COMPOSER: u32 = 9;
    pub const ALL: [u32; 3] = [GFXSTREAM_VULKAN, GFXSTREAM_GLES, GFXSTREAM_COMPOSER];
}

/// `RendererFlags`.
pub mod flags {
    pub const USE_EGL: u64 = 1 << 0;
    pub const USE_GLES: u64 = 1 << 4;
    pub const USE_VK: u64 = 1 << 5;
    /// Host-visible blobs allocated so they can be exported as fds ([`Renderer::export_blob`]):
    /// what a frontend in another process needs to map them into its guest.
    pub const USE_EXTERNAL_BLOB: u64 = 1 << 6;
    /// Host-visible blobs from system memory (shm) rather than the Vulkan driver.
    pub const USE_SYSTEM_BLOB: u64 = 1 << 7;
    /// Vulkan composition and posting. On macOS the GL post worker hands work to an
    /// emulator-UI-thread hook that standalone hosts don't service (compose never completes).
    pub const VULKAN_NATIVE_SWAPCHAIN: u64 = 1 << 8;
}

/// `stream_renderer_handle.handle_type` of an exported blob.
pub mod handle {
    pub const MEM_OPAQUE_FD: u32 = 0x1;
    pub const MEM_DMABUF: u32 = 0x2;
    pub const MEM_SHM: u32 = 0x4;
}

pub const FENCE_FLAG: u32 = 1 << 0;
pub const FENCE_FLAG_RING_IDX: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fence {
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceCreateArgs {
    pub handle: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Box3 {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

#[repr(C)]
struct Iov {
    base: *mut c_void,
    len: usize,
}

#[repr(C)]
struct Param {
    key: u64,
    value: u64,
}

#[repr(C)]
struct Command {
    ctx_id: u32,
    cmd_size: u32,
    cmd: *mut u8,
    num_in_fences: u32,
    fences: *mut c_void,
}

#[repr(C)]
struct CreateBlob {
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct Handle {
    os_handle: i64,
    handle_type: u32,
}

#[repr(C)]
struct Debug {
    debug_type: u32,
    message: *const c_char,
}

type FenceCb = extern "C" fn(*mut c_void, *mut Fence);
type DebugCb = extern "C" fn(*mut c_void, *mut Debug);

struct Api {
    init: unsafe extern "C" fn(*mut Param, u64) -> c_int,
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    context_create: unsafe extern "C" fn(u32, u32, *const c_char, u32) -> c_int,
    context_destroy: unsafe extern "C" fn(u32),
    resource_create: unsafe extern "C" fn(*mut ResourceCreateArgs, *mut Iov, u32) -> c_int,
    resource_unref: unsafe extern "C" fn(u32),
    attach_iov: unsafe extern "C" fn(c_int, *mut Iov, c_int) -> c_int,
    detach_iov: unsafe extern "C" fn(c_int, *mut *mut Iov, *mut c_int),
    ctx_attach_resource: unsafe extern "C" fn(c_int, c_int),
    ctx_detach_resource: unsafe extern "C" fn(c_int, c_int),
    submit_cmd: unsafe extern "C" fn(*mut Command) -> c_int,
    transfer_read_iov: unsafe extern "C" fn(u32, u32, u32, u32, u32, *mut Box3, u64, *mut Iov, c_int) -> c_int,
    transfer_write_iov: unsafe extern "C" fn(u32, u32, c_int, u32, u32, *mut Box3, u64, *mut Iov, u32) -> c_int,
    create_blob: unsafe extern "C" fn(u32, u32, *const CreateBlob, *const Iov, u32, *const c_void) -> c_int,
    resource_map: unsafe extern "C" fn(u32, *mut *mut c_void, *mut u64) -> c_int,
    resource_unmap: unsafe extern "C" fn(u32) -> c_int,
    resource_map_info: unsafe extern "C" fn(u32, *mut u32) -> c_int,
    export_blob: unsafe extern "C" fn(u32, *mut Handle) -> c_int,
    create_fence: unsafe extern "C" fn(*const Fence) -> c_int,
    suspend: unsafe extern "C" fn() -> c_int,
    snapshot: unsafe extern "C" fn(*const c_char) -> c_int,
    restore: unsafe extern "C" fn(*const c_char) -> c_int,
    resume: unsafe extern "C" fn() -> c_int,
}

fn dlerror() -> String {
    // SAFETY: dlerror returns a thread-local C string or null.
    unsafe {
        let e = libc::dlerror();
        if e.is_null() { "unknown error".into() } else { std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned() }
    }
}

fn dlopen(path: &Path, global: bool) -> Result<*mut c_void> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| Error::Load(path.display().to_string(), e.to_string()))?;
    let mode = libc::RTLD_NOW | if global { libc::RTLD_GLOBAL } else { libc::RTLD_LOCAL };
    // SAFETY: valid NUL-terminated path; the handle is intentionally never closed.
    let h = unsafe { libc::dlopen(c.as_ptr(), mode) };
    if h.is_null() { Err(Error::Load(path.display().to_string(), dlerror())) } else { Ok(h) }
}

macro_rules! sym {
    // A name chosen per platform (a `&'static str` constant without the NUL).
    ($h:expr, @$name:expr) => {{
        let name: &'static str = $name;
        let Ok(c) = CString::new(name) else { return Err(Error::Symbol(name)) };
        // SAFETY: NUL-terminated name; the transmute target matches the C++ member's prototype
        // (`this` first, Itanium ABI).
        unsafe {
            let p = libc::dlsym($h, c.as_ptr());
            if p.is_null() {
                return Err(Error::Symbol(name));
            }
            std::mem::transmute::<*mut c_void, _>(p)
        }
    }};
    ($h:expr, $name:literal) => {{
        // SAFETY: the symbol name is a NUL-terminated literal; the transmute target matches the
        // C prototype in virtio-gpu-gfxstream-renderer.h.
        unsafe {
            let p = libc::dlsym($h, concat!($name, "\0").as_ptr().cast());
            if p.is_null() {
                return Err(Error::Symbol($name));
            }
            std::mem::transmute::<*mut c_void, _>(p)
        }
    }};
}

impl Api {
    fn load(h: *mut c_void) -> Result<Api> {
        Ok(Api {
            init: sym!(h, "stream_renderer_init"),
            get_cap_set: sym!(h, "stream_renderer_get_cap_set"),
            fill_caps: sym!(h, "stream_renderer_fill_caps"),
            context_create: sym!(h, "stream_renderer_context_create"),
            context_destroy: sym!(h, "stream_renderer_context_destroy"),
            resource_create: sym!(h, "stream_renderer_resource_create"),
            resource_unref: sym!(h, "stream_renderer_resource_unref"),
            attach_iov: sym!(h, "stream_renderer_resource_attach_iov"),
            detach_iov: sym!(h, "stream_renderer_resource_detach_iov"),
            ctx_attach_resource: sym!(h, "stream_renderer_ctx_attach_resource"),
            ctx_detach_resource: sym!(h, "stream_renderer_ctx_detach_resource"),
            submit_cmd: sym!(h, "stream_renderer_submit_cmd"),
            transfer_read_iov: sym!(h, "stream_renderer_transfer_read_iov"),
            transfer_write_iov: sym!(h, "stream_renderer_transfer_write_iov"),
            create_blob: sym!(h, "stream_renderer_create_blob"),
            resource_map: sym!(h, "stream_renderer_resource_map"),
            resource_unmap: sym!(h, "stream_renderer_resource_unmap"),
            resource_map_info: sym!(h, "stream_renderer_resource_map_info"),
            export_blob: sym!(h, "stream_renderer_export_blob"),
            create_fence: sym!(h, "stream_renderer_create_fence"),
            suspend: sym!(h, "stream_renderer_suspend"),
            snapshot: sym!(h, "stream_renderer_snapshot"),
            restore: sym!(h, "stream_renderer_restore"),
            resume: sym!(h, "stream_renderer_resume"),
        })
    }
}

type FenceHandler = Arc<dyn Fn(Fence) + Send + Sync>;
static FENCE_HANDLER: OnceLock<FenceHandler> = OnceLock::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_fence(_: *mut c_void, fence: *mut Fence) {
    if fence.is_null() {
        return;
    }
    // SAFETY: the renderer passes a valid fence for the duration of the call.
    let f = unsafe { *fence };
    if let Some(h) = FENCE_HANDLER.get() {
        h(f);
    }
}

extern "C" fn on_debug(_: *mut c_void, d: *mut Debug) {
    if d.is_null() {
        return;
    }
    // SAFETY: valid for the duration of the call; message is NUL-terminated.
    let (kind, msg) = unsafe { ((*d).debug_type, std::ffi::CStr::from_ptr((*d).message).to_string_lossy().into_owned()) };
    if msg.contains("Failed to initialize global Vulkan emulation") {
        VULKAN_FAILED.store(true, Ordering::Relaxed);
    }
    // 1 error, 2 warn, 3 info, 4 debug.
    if kind <= 2 || (kind == 3 && VERBOSE.load(Ordering::Relaxed)) {
        eprintln!("gfxstream: {}", msg.trim_end());
    }
}

fn iovs(ranges: &[HostRange]) -> Vec<Iov> {
    ranges.iter().map(|r| Iov { base: r.as_ptr().cast(), len: r.len() }).collect()
}

/// A resource's guest memory as the renderer's iovec array, built in place from host ranges
/// (one allocation, no intermediate list). [`Renderer::attach_backing`] and
/// [`Renderer::create_blob`] take it and keep it for as long as the renderer may hold it.
#[derive(Default)]
pub struct Backing(Vec<Iov>);

impl Backing {
    pub fn with_capacity(n: usize) -> Backing {
        Backing(Vec::with_capacity(n))
    }

    pub fn push(&mut self, r: HostRange) {
        self.0.push(Iov { base: r.as_ptr().cast(), len: r.len() });
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The iovec arrays handed to the renderer, owned until it can no longer use them. The header
/// doesn't say whether the renderer copies them; the SDK's (37.x) `VirtioGpuResource::AttachIov`
/// copies the array and its `createBlob` ignores it, but older gfxstream and other renderers may
/// keep the pointer, so (like crosvm's rutabaga, which keeps `backing_iovecs` in the resource)
/// an array is freed only after the call that ends its use returns: a detach or a re-attach for
/// attached backing, the resource's unref for both.
#[derive(Default)]
struct IovStore {
    attached: HashMap<u32, Box<[Iov]>>,
    blobs: HashMap<u32, Box<[Iov]>>,
}

impl IovStore {
    /// Keeps `v` as `res`'s attached backing. The array it replaces is no longer the renderer's
    /// (the attach that replaced it has returned) and is returned to be dropped.
    fn attach(&mut self, res: u32, v: Box<[Iov]>) -> Option<Box<[Iov]>> {
        if v.is_empty() { self.attached.remove(&res) } else { self.attached.insert(res, v) }
    }

    fn detach(&mut self, res: u32) -> Option<Box<[Iov]>> {
        self.attached.remove(&res)
    }

    fn blob(&mut self, res: u32, v: Box<[Iov]>) {
        if !v.is_empty() {
            self.blobs.insert(res, v);
        }
    }

    /// Drops everything kept for `res` (after its unref); returns how many arrays that was.
    fn release(&mut self, res: u32) -> usize {
        usize::from(self.attached.remove(&res).is_some()) + usize::from(self.blobs.remove(&res).is_some())
    }

    /// Iovec entries kept, over every array.
    fn entries(&self) -> usize {
        self.attached.values().chain(self.blobs.values()).map(|v| v.len()).sum()
    }
}

fn ok(name: &'static str, rc: c_int) -> Result<()> {
    if rc == 0 { Ok(()) } else { Err(Error::Call(name, rc)) }
}

/// The initialized renderer, shared between threads. The header promises no more than that its
/// calls may come from other threads than `init`'s; it doesn't say the resource table may be used
/// by two at once, so callers make every resource call (create, unref, backing, transfers,
/// submits, maps) from one thread, as virtio-gpu's worker does. The FrameBuffer readbacks
/// ([`Renderer::read_last_posted_to`] and friends) take the FrameBuffer's own lock and may come
/// from any thread.
pub struct Renderer {
    api: Api,
    handle: *mut c_void,
    /// The backend library, for symbols `dlsym` can't see.
    backend: PathBuf,
    /// FrameBuffer members, resolved on first use (readback per streamed frame, release per
    /// freed buffer), not per call.
    fb: std::sync::OnceLock<std::result::Result<FbApi, &'static str>>,
    /// The direct Vulkan readback, resolved on first use; `None` when this renderer doesn't have
    /// it in the expected shape.
    vk: std::sync::OnceLock<Option<VkRead>>,
    /// [`VkTrust`] of the direct readback, as a u8.
    vk_trust: AtomicU8,
    /// The renderer has a GL backend (initialized with [`flags::USE_GLES`]). Without one, color
    /// buffers are Vulkan images only and every GL path (the exported readback, the post
    /// callback's readback) aborts in the renderer, so they're never taken.
    gl: AtomicBool,
    /// Iovec arrays the renderer may still point at.
    iovs: Mutex<IovStore>,
    /// The Metal present path ([`Renderer::last_posted_surface`]), set up on first use; `None`
    /// when this renderer doesn't have it in the expected shape.
    #[cfg(target_os = "macos")]
    metal: OnceLock<Option<MetalPath>>,
    /// The last post the display was told of, with its size ([`Renderer::note_post`]).
    post: metal::LastPost,
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // The renderer is process-wide state that outlives this handle (there is no teardown
        // call), so arrays it may still point at stay allocated.
        std::mem::forget(std::mem::take(&mut *self.iov_store()));
    }
}

// SAFETY: the dlopen handle is only used for dlsym, which is thread-safe.
unsafe impl Send for Renderer {}
// SAFETY: as above; the renderer's own state is process-wide and its calls may come from any
// thread; resource calls are kept on one thread by the caller (see `Renderer`), and the iovec
// store is behind its own mutex.
unsafe impl Sync for Renderer {}

/// The renderer library inside `lib64`.
#[cfg(target_os = "macos")]
pub const BACKEND: &str = "libgfxstream_backend.dylib";
#[cfg(target_os = "linux")]
pub const BACKEND: &str = "libgfxstream_backend.so";

/// Libraries in `lib64` the backend `dlopen`s by bare name at run time. dyld finds them through
/// the executable's rpath (`@executable_path/lib64`, see [`link_beside_exe`]); glibc's `dlopen`
/// matches a bare name against libraries already loaded, so on Linux they're preloaded. That
/// includes an EGL and GLES: the backend opens `libEGL.so` and `libGLESv2.so` during init even
/// without a GL backend, and crashes when neither loads. ANGLE's (`gles_angle`) carry exactly
/// those sonames, and ANGLE is the GLES emu gives the renderer on macOS too.
#[cfg(target_os = "linux")]
const DLOPENED: &[&str] = &["libshadertranslator.so", "gles_angle/libGLESv2.so", "gles_angle/libEGL.so"];
#[cfg(target_os = "macos")]
const DLOPENED: &[&str] = &[];

/// The `lib64` libraries `path` links at load time, by file name.
fn linked_names(path: &Path) -> Vec<String> {
    #[cfg(target_os = "macos")]
    let names = macho::dylib_deps(path)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|dep| dep.strip_prefix("@rpath/").or_else(|| dep.strip_prefix("@loader_path/")).map(str::to_string))
        .collect();
    // DT_NEEDED holds bare sonames; the renderer's own ones are files in lib64, the system's
    // (libc, libX11, ...) aren't and are left to the loader.
    #[cfg(target_os = "linux")]
    let names = elf::needed(path).unwrap_or_default();
    names
}

/// The libraries `root` (a file in `lib64`) links from `lib64` itself, directly or indirectly,
/// dependencies before their dependents; `root` is not included. Only references to files
/// present in `lib64` count: `@rpath/<name>` and `@loader_path/<name>` on macOS, `DT_NEEDED`
/// names on Linux.
pub fn bundled_deps(lib64: &Path, root: &str) -> Vec<PathBuf> {
    fn visit(lib64: &Path, name: &str, seen: &mut HashSet<String>, out: &mut Vec<PathBuf>) {
        if !seen.insert(name.to_string()) {
            return;
        }
        let path = lib64.join(name);
        for dep in linked_names(&path) {
            if !dep.contains('/') && lib64.join(&dep).is_file() {
                visit(lib64, &dep, seen, out);
            }
        }
        out.push(path);
    }
    let mut out = Vec::new();
    visit(lib64, root, &mut HashSet::new(), &mut out);
    out.pop();
    out
}

/// What [`Renderer::load`] loads before the backend, in order: the backend's bundled
/// dependencies, then each library it `dlopen`s by bare name ([`DLOPENED`]) with its own.
pub fn preloads(lib64: &Path) -> Vec<PathBuf> {
    let mut out = bundled_deps(lib64, BACKEND);
    for name in DLOPENED.iter().filter(|n| lib64.join(n).is_file()) {
        for p in bundled_deps(lib64, name).into_iter().chain([lib64.join(name)]) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

impl Renderer {
    /// Loads the renderer from a `lib64` directory: the SDK's `emulator/lib64` or one installed by
    /// `emu -provision-renderer`. The backend's dependencies are preloaded from the same
    /// directory ([`preloads`]), leaves first, so the loader finds each one already loaded by
    /// install name (dyld) or soname (glibc) and the executable needs no rpath to it.
    pub fn load(lib64: &Path) -> Result<Renderer> {
        for dep in preloads(lib64) {
            dlopen(&dep, true)?;
        }
        let backend = lib64.join(BACKEND);
        let h = dlopen(&backend, false)?;
        Ok(Renderer {
            api: Api::load(h)?,
            handle: h,
            backend,
            fb: std::sync::OnceLock::new(),
            vk: std::sync::OnceLock::new(),
            vk_trust: AtomicU8::new(VkTrust::Unchecked as u8),
            gl: AtomicBool::new(true),
            iovs: Mutex::new(IovStore::default()),
            #[cfg(target_os = "macos")]
            metal: OnceLock::new(),
            post: metal::LastPost::default(),
        })
    }

    /// Initializes the renderer for a `width`×`height` primary display. `on_fence` runs on a
    /// renderer thread whenever a fence created with [`Renderer::create_fence`] signals.
    pub fn init(&self, width: u32, height: u32, renderer_flags: u64, verbose: bool, on_fence: FenceHandler) -> Result<()> {
        self.init_with_features(width, height, renderer_flags, None, verbose, on_fence)
    }

    /// Like [`Renderer::init`], with feature overrides: `"Name:enabled,Other:disabled"`.
    pub fn init_with_features(
        &self,
        width: u32,
        height: u32,
        renderer_flags: u64,
        features: Option<&str>,
        verbose: bool,
        on_fence: FenceHandler,
    ) -> Result<()> {
        if INITIALIZED.swap(true, Ordering::SeqCst) {
            return Err(Error::AlreadyInitialized);
        }
        VERBOSE.store(verbose, Ordering::Relaxed);
        let _ = FENCE_HANDLER.set(on_fence);
        let gl = renderer_flags & flags::USE_GLES != 0;
        self.gl.store(gl, Ordering::Relaxed);
        if !gl {
            // Nothing to check the direct readback against, and nothing else to read with.
            self.vk_trust.store(VkTrust::Trusted as u8, Ordering::Relaxed);
        }
        let features = features.map(|f| CString::new(f).unwrap_or_default());
        let mut params = vec![
            Param { key: 1, value: 0 },
            Param { key: 2, value: renderer_flags },
            Param { key: 3, value: on_fence_ptr() },
            Param { key: 4, value: u64::from(width) },
            Param { key: 5, value: u64::from(height) },
            Param { key: 6, value: on_debug_ptr() },
        ];
        if let Some(f) = &features {
            params.push(Param { key: 11, value: f.as_ptr() as usize as u64 });
        }
        // SAFETY: params (and the features string) outlive the call; callbacks are 'static.
        ok("stream_renderer_init", unsafe { (self.api.init)(params.as_mut_ptr(), params.len() as u64) })
    }

    /// (max version, max size) of a capability set, or None if unsupported.
    pub fn capset_info(&self, id: u32) -> Option<(u32, u32)> {
        let (mut ver, mut size) = (0u32, 0u32);
        // SAFETY: out-pointers reference live locals.
        unsafe { (self.api.get_cap_set)(id, &mut ver, &mut size) };
        (size > 0).then_some((ver, size))
    }

    pub fn capset(&self, id: u32, version: u32) -> Vec<u8> {
        let Some((_, size)) = self.capset_info(id) else { return Vec::new() };
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf has exactly the size the renderer reported for this capset.
        unsafe { (self.api.fill_caps)(id, version, buf.as_mut_ptr().cast()) };
        buf
    }

    pub fn context_create(&self, ctx_id: u32, name: &[u8], context_init: u32) -> Result<()> {
        // SAFETY: name pointer/length pair describes a live slice.
        ok("context_create", unsafe { (self.api.context_create)(ctx_id, name.len() as u32, name.as_ptr().cast(), context_init) })
    }

    pub fn context_destroy(&self, ctx_id: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.context_destroy)(ctx_id) }
    }

    fn iov_store(&self) -> MutexGuard<'_, IovStore> {
        self.iovs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Iovec entries kept for the renderer (attached backing and guest-memory blobs).
    pub fn backing_entries(&self) -> usize {
        self.iov_store().entries()
    }

    /// Creates a resource, optionally backed by guest memory ranges (kept as its attached
    /// backing, like [`Renderer::attach_backing`]).
    pub fn resource_create(&self, mut args: ResourceCreateArgs, backing: &[HostRange]) -> Result<()> {
        let mut v = iovs(backing).into_boxed_slice();
        let p = if v.is_empty() { std::ptr::null_mut() } else { v.as_mut_ptr() };
        // SAFETY: the iovecs point into guest RAM, which outlives the renderer; the array is kept
        // until the resource's detach or unref.
        let rc = unsafe { (self.api.resource_create)(&mut args, p, v.len() as u32) };
        if rc == 0 {
            drop(self.iov_store().attach(args.handle, v));
        }
        ok("resource_create", rc)
    }

    /// Unrefs a resource and then frees the iovec arrays kept for it.
    pub fn resource_unref(&self, id: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.resource_unref)(id) }
        self.iov_store().release(id);
    }

    /// Attaches guest memory to a resource. The array is kept until the resource's next
    /// attach, its detach or its unref returns.
    pub fn attach_backing(&self, id: u32, backing: Backing) -> Result<()> {
        let mut v = backing.0.into_boxed_slice();
        // SAFETY: the array points into guest RAM and stays allocated (in `iovs`) for as long as
        // the renderer may use it.
        let rc = unsafe { (self.api.attach_iov)(id as c_int, v.as_mut_ptr(), v.len() as c_int) };
        if rc == 0 {
            // The previous array (if any) is dropped only now, after the renderer replaced it.
            drop(self.iov_store().attach(id, v));
        }
        ok("attach_iov", rc)
    }

    /// Detaches a resource's guest memory and then frees its iovec array.
    pub fn detach_backing(&self, id: u32) {
        let (mut p, mut n) = (std::ptr::null_mut(), 0);
        // SAFETY: out-pointers reference live locals.
        unsafe { (self.api.detach_iov)(id as c_int, &mut p, &mut n) }
        drop(self.iov_store().detach(id));
    }

    pub fn ctx_attach_resource(&self, ctx: u32, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.ctx_attach_resource)(ctx as c_int, res as c_int) }
    }

    pub fn ctx_detach_resource(&self, ctx: u32, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.ctx_detach_resource)(ctx as c_int, res as c_int) }
    }

    pub fn submit(&self, ctx: u32, cmd: &mut [u8]) -> Result<()> {
        let mut c =
            Command { ctx_id: ctx, cmd_size: cmd.len() as u32, cmd: cmd.as_mut_ptr(), num_in_fences: 0, fences: std::ptr::null_mut() };
        // SAFETY: the command buffer lives for the call.
        ok("submit_cmd", unsafe { (self.api.submit_cmd)(&mut c) })
    }

    /// Copies resource contents into `dst` (guest backing when `dst` is empty).
    #[allow(clippy::too_many_arguments)]
    pub fn transfer_read(
        &self,
        res: u32,
        ctx: u32,
        level: u32,
        stride: u32,
        layer_stride: u32,
        mut b: Box3,
        offset: u64,
        dst: &[HostRange],
    ) -> Result<()> {
        let mut v = iovs(dst);
        let p = if v.is_empty() { std::ptr::null_mut() } else { v.as_mut_ptr() };
        // SAFETY: iovecs (if any) point into memory valid for the call.
        ok("transfer_read_iov", unsafe {
            (self.api.transfer_read_iov)(res, ctx, level, stride, layer_stride, &mut b, offset, p, v.len() as c_int)
        })
    }

    /// Reads into a host buffer (e.g. a scanout frame).
    #[allow(clippy::too_many_arguments)]
    pub fn read_into(&self, res: u32, stride: u32, b: Box3, out: &mut [u8]) -> Result<()> {
        // SAFETY: `out` is a live, exclusively borrowed buffer for the duration of the call.
        let r = unsafe { HostRange::from_raw(out.as_mut_ptr(), out.len()) };
        self.transfer_read(res, 0, 0, stride, 0, b, 0, &[r])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transfer_write(&self, res: u32, ctx: u32, level: u32, stride: u32, layer_stride: u32, mut b: Box3, offset: u64) -> Result<()> {
        // SAFETY: no iovecs: the renderer uses the resource's attached guest backing.
        ok("transfer_write_iov", unsafe {
            (self.api.transfer_write_iov)(res, ctx, level as c_int, stride, layer_stride, &mut b, offset, std::ptr::null_mut(), 0)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_blob(&self, ctx: u32, res: u32, blob_mem: u32, blob_flags: u32, blob_id: u64, size: u64, backing: Backing) -> Result<()> {
        let b = CreateBlob { blob_mem, blob_flags, blob_id, size };
        let v = backing.0.into_boxed_slice();
        let p = if v.is_empty() { std::ptr::null() } else { v.as_ptr() };
        // SAFETY: the blob descriptor lives for the call; the iovec array points into guest RAM
        // and is kept until the resource's unref, since the renderer may keep it for
        // guest-memory blobs.
        let rc = unsafe { (self.api.create_blob)(ctx, res, &b, p, v.len() as u32, std::ptr::null()) };
        if rc == 0 {
            self.iov_store().blob(res, v);
        }
        ok("create_blob", rc)
    }

    /// Host mapping of a mappable blob.
    pub fn map(&self, res: u32) -> Result<HostRange> {
        let (mut p, mut size) = (std::ptr::null_mut(), 0u64);
        // SAFETY: out-pointers reference live locals.
        ok("resource_map", unsafe { (self.api.resource_map)(res, &mut p, &mut size) })?;
        // SAFETY: the renderer keeps the mapping alive until resource_unmap/unref.
        Ok(unsafe { HostRange::from_raw(p.cast(), size as usize) })
    }

    pub fn unmap(&self, res: u32) -> Result<()> {
        // SAFETY: plain call.
        ok("resource_unmap", unsafe { (self.api.resource_unmap)(res) })
    }

    /// A host-visible blob's memory as a file descriptor of the given [`handle`] type, for
    /// another process to map (a vhost-user frontend's `SHMEM_MAP`). Needs the renderer
    /// initialized with [`flags::USE_EXTERNAL_BLOB`] or [`flags::USE_SYSTEM_BLOB`]. The renderer
    /// hands over the blob's own descriptor and keeps none (gfxstream's
    /// `VirtioGpuResource::ExportBlob` releases it), so a blob exports once: keep the fd for as
    /// long as the blob may be mapped again.
    pub fn export_blob(&self, res: u32) -> Result<(std::os::fd::OwnedFd, u32)> {
        use std::os::fd::FromRawFd;
        let mut h = Handle::default();
        // SAFETY: out-pointer references a live local.
        ok("export_blob", unsafe { (self.api.export_blob)(res, &mut h) })?;
        let fd = std::os::fd::RawFd::try_from(h.os_handle).map_err(|_| Error::Call("export_blob: handle", -22))?;
        if fd < 0 {
            return Err(Error::Call("export_blob: no fd", -9));
        }
        // SAFETY: the renderer releases the blob's descriptor to us (it keeps no copy), so this
        // is its only owner from here on.
        Ok((unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }, h.handle_type))
    }

    pub fn map_info(&self, res: u32) -> u32 {
        let mut info = 0u32;
        // SAFETY: out-pointer references a live local.
        let _ = unsafe { (self.api.resource_map_info)(res, &mut info) };
        info
    }

    pub fn create_fence(&self, f: Fence) -> Result<()> {
        // SAFETY: the fence lives for the call.
        ok("create_fence", unsafe { (self.api.create_fence)(&f) })
    }

    /// Stops renderer work ahead of a snapshot (the guest must be paused).
    pub fn suspend(&self) -> Result<()> {
        // SAFETY: no arguments.
        ok("suspend", unsafe { (self.api.suspend)() })
    }

    /// Writes the renderer's state into directory `dir`.
    pub fn snapshot(&self, dir: &Path) -> Result<()> {
        let c = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| Error::Call("snapshot", -22))?;
        // SAFETY: NUL-terminated path valid for the call.
        ok("snapshot", unsafe { (self.api.snapshot)(c.as_ptr()) })
    }

    /// Loads the renderer's state from `dir` (before the guest runs).
    pub fn restore(&self, dir: &Path) -> Result<()> {
        let c = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| Error::Call("restore", -22))?;
        // SAFETY: NUL-terminated path valid for the call.
        ok("restore", unsafe { (self.api.restore)(c.as_ptr()) })
    }

    pub fn resume(&self) -> Result<()> {
        // SAFETY: no arguments.
        ok("resume", unsafe { (self.api.resume)() })
    }
}

fn on_fence_ptr() -> u64 {
    on_fence as FenceCb as usize as u64
}

fn on_debug_ptr() -> u64 {
    on_debug as DebugCb as usize as u64
}

/// Host Vulkan driver for the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icd {
    /// Metal, on the host GPU.
    MoltenVk,
    /// Metal, on the host GPU, through Mesa's KosmicKrisp (the SDK ships it beside MoltenVK).
    KosmicKrisp,
    /// Software (llvmpipe); always works, uses CPU.
    Lavapipe,
    SwiftShader,
}

impl Icd {
    pub fn manifest(self) -> &'static str {
        match self {
            Icd::MoltenVk => "MoltenVK_icd.json",
            Icd::KosmicKrisp => "libkosmickrisp_icd.json",
            Icd::Lavapipe => "lvp_icd.json",
            Icd::SwiftShader => "vk_swiftshader_icd.json",
        }
    }

    /// The name gfxstream takes in `ANDROID_EMU_VK_ICD`.
    pub fn name(self) -> &'static str {
        match self {
            Icd::MoltenVk => "moltenvk",
            Icd::KosmicKrisp => "kosmickrisp",
            Icd::Lavapipe => "lavapipe",
            Icd::SwiftShader => "swiftshader",
        }
    }

    /// The ICD `name` ([`Icd::name`]) stands for.
    pub fn from_name(name: &str) -> Option<Icd> {
        [Icd::MoltenVk, Icd::KosmicKrisp, Icd::Lavapipe, Icd::SwiftShader].into_iter().find(|icd| icd.name() == name)
    }

    /// Whether the renderer's `lib64` ships this ICD's manifest.
    pub fn ships_in(self, lib64: &Path) -> bool {
        lib64.join("vulkan").join(self.manifest()).is_file()
    }
}

/// The host-GPU Vulkan driver for a boot whose `-gpu` mode is the host GPU: KosmicKrisp when
/// the guest's UI draws with Skia GL on the guest's own ANGLE (`ui_gl_on_angle`) and the
/// renderer ships KosmicKrisp, else MoltenVK. `asked` (`ANDROID_EMU_VK_ICD`) naming either GPU
/// driver wins, KosmicKrisp only where it ships; any other name leaves the choice here.
///
/// That ANGLE (API 34 and 35 images) builds pipelines whose vertex input format has another
/// numeric type than the shader's input (a float format for a `uvec2`): Vulkan leaves the values
/// undefined and compiles them, Metal refuses ("Vertex attribute ... of type uint2 cannot be
/// read using MTLAttributeFormatFloat4"), so MoltenVK fails each one and the draw is lost.
/// No MoltenVK setting changes that; KosmicKrisp compiles them (issue #412).
pub fn gpu_icd(ui_gl_on_angle: bool, kosmickrisp_ships: bool, asked: Option<&str>) -> Icd {
    match asked.and_then(Icd::from_name) {
        Some(Icd::MoltenVk) => Icd::MoltenVk,
        Some(Icd::KosmicKrisp) if kosmickrisp_ships => Icd::KosmicKrisp,
        Some(Icd::KosmicKrisp) => Icd::MoltenVk,
        _ if ui_gl_on_angle && kosmickrisp_ships => Icd::KosmicKrisp,
        _ => Icd::MoltenVk,
    }
}

/// Whether the renderer brought up Vulkan (false after "Failed to initialize global Vulkan").
pub fn vulkan_ok() -> bool {
    !VULKAN_FAILED.load(Ordering::Relaxed)
}

static VULKAN_FAILED: AtomicBool = AtomicBool::new(false);

/// Points the renderer at its Vulkan loader and ICDs (MoltenVK by default). gfxstream resolves
/// `lib64/vulkan/*` and `lib64/gles_*` relative to its "launcher directory", which defaults to
/// the executable's directory; set it to the parent of the renderer's `lib64` (the SDK's
/// `emulator/`, or `<sdk>/simee/renderer/<revision>/`).
///
/// The variables that pick the driver ([`driver_environment`]) are always set to `icd`'s; the
/// rest only where unset. Returns what a user should hear: an `ANDROID_EMU_VK_ICD` (or loader manifest)
/// of theirs honoured or overridden.
///
/// Call before spawning threads that might read the environment.
pub fn configure_environment(emulator_dir: &Path, icd: Icd) -> Vec<String> {
    let (set, notices) = applied(emulator_dir, icd, |k| std::env::var_os(k));
    for (k, v) in set {
        // SAFETY: documented precondition: called while the process is single-threaded with
        // respect to environment access.
        unsafe { std::env::set_var(k, v) };
    }
    notices
}

/// What [`configure_environment`] sets given the environment as it stands (`current`), and the
/// notices it returns: the driver's variables ([`driver_environment`]) always, so gfxstream
/// loads the driver the caller chose (a user's `ANDROID_EMU_VK_ICD` is honoured when it names
/// that driver and overridden, with a notice, when it doesn't), and every other variable only
/// where it isn't set (the user's MoltenVK or ANGLE settings win).
fn applied(emulator_dir: &Path, icd: Icd, current: impl Fn(&str) -> Option<OsString>) -> (Vec<(&'static str, OsString)>, Vec<String>) {
    let driver: Vec<&str> = driver_environment(emulator_dir, icd).into_iter().map(|(k, _)| k).collect();
    let (mut set, mut notices) = (Vec::new(), Vec::new());
    for (k, v) in environment(emulator_dir, icd) {
        match current(k) {
            None => set.push((k, v)),
            Some(user) if driver.contains(&k) => {
                if user != v {
                    notices.push(format!(
                        "emu: {k}={} overridden: gfxstream gets {}, the Vulkan driver emu chose ({})",
                        user.to_string_lossy(),
                        v.to_string_lossy(),
                        icd.name()
                    ));
                } else if k == "ANDROID_EMU_VK_ICD" {
                    notices.push(format!("emu: ANDROID_EMU_VK_ICD={} honoured", icd.name()));
                }
                set.push((k, v));
            }
            Some(_) => {}
        }
    }
    (set, notices)
}

/// The variables that pick the Vulkan driver gfxstream loads, for `icd`: the loader's manifest
/// (the loader otherwise searches only system ICD directories, found empty on macOS) and
/// gfxstream's own `ANDROID_EMU_VK_ICD`.
pub fn driver_environment(emulator_dir: &Path, icd: Icd) -> Vec<(&'static str, OsString)> {
    let manifest = emulator_dir.join("lib64/vulkan").join(icd.manifest());
    let mut vars = Vec::new();
    if manifest.is_file() {
        vars.push(("VK_DRIVER_FILES", manifest.clone().into_os_string()));
        vars.push(("VK_ICD_FILENAMES", manifest.into_os_string()));
    }
    vars.push(("ANDROID_EMU_VK_ICD", icd.name().into()));
    vars
}

/// Everything [`configure_environment`] sets for `icd`.
fn environment(emulator_dir: &Path, icd: Icd) -> Vec<(&'static str, OsString)> {
    let vk = emulator_dir.join("lib64/vulkan");
    let loader = vk.join(if cfg!(target_os = "macos") { "libvulkan.dylib" } else { "libvulkan.so.1" });
    let mut vars = vec![("ANDROID_EMULATOR_LAUNCHER_DIR", emulator_dir.as_os_str().to_owned())];
    if loader.is_file() {
        vars.push(("ANDROID_EMU_VK_LOADER_PATH", loader.into_os_string()));
    }
    vars.extend(driver_environment(emulator_dir, icd));
    // Host GLES through ANGLE (Metal) instead of the deprecated desktop GL, which lacks GLSL ES 3.
    vars.push(("ANDROID_EGL_ON_EGL", "1".into()));
    // ANGLE's default display on macOS is its CGL backend, whose context setup for a
    // ColorBuffer's display surface hangs inside libGL (blocking that render thread and with it
    // the guest's pipe reads). Metal is what the backend is meant to run on here.
    if cfg!(target_os = "macos") {
        vars.push(("ANGLE_DEFAULT_PLATFORM", "metal".into()));
    }
    if icd == Icd::MoltenVk {
        // What gfxstream configures for MoltenVK: its compositor shaders fail to convert with
        // Metal argument buffers on.
        vars.push(("MVK_CONFIG_USE_METAL_ARGUMENT_BUFFERS", "0".into()));
        vars.push(("MVK_CONFIG_VK_SEMAPHORE_SUPPORT_STYLE", "0".into()));
        vars.push(("MVK_CONFIG_USE_MTLHEAP", "1".into()));
    }
    vars
}

/// Runs the renderer's host GLES (ANGLE) on `platform` (`metal`, `swiftshader`, ...) instead of
/// what [`configure_environment`] picks, unless `ANGLE_DEFAULT_PLATFORM` is already set (the
/// user's choice wins). Call before [`configure_environment`], under the same precondition.
pub fn prefer_angle_platform(platform: &str) {
    if std::env::var_os("ANGLE_DEFAULT_PLATFORM").is_none() {
        // SAFETY: documented precondition: called while the process is single-threaded with
        // respect to environment access.
        unsafe { std::env::set_var("ANGLE_DEFAULT_PLATFORM", platform) };
    }
}

/// Makes `<exe dir>/lib64` lead to `lib64`. gfxstream `dlopen`s some libraries by bare name
/// (`libshadertranslator.dylib`), which dyld resolves only through the executable's rpaths, one
/// of which is `@executable_path/lib64` (`apps/emu/build.rs`). In an SDK overlay `lib64` is
/// already there (a real directory, left alone); for a build in `target/` a symlink is created,
/// or repointed when it leads to another renderer (an old revision, another SDK). Best effort:
/// skipped if the directory isn't writable. Returns the link when it leads to `lib64`.
pub fn link_beside_exe(lib64: &Path) -> Option<PathBuf> {
    let link = std::env::current_exe().ok()?.parent()?.join("lib64");
    match std::fs::read_link(&link) {
        Ok(target) if target == lib64 => return Some(link),
        Ok(_) => {
            // Replace atomically: a concurrent emu may be resolving through the old link.
            let tmp = link.with_file_name(format!(".lib64.{}", std::process::id()));
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(lib64, &tmp).ok()?;
            std::fs::rename(&tmp, &link).ok()?;
        }
        Err(_) if link.symlink_metadata().is_ok() => return None, // a real directory
        Err(_) => std::os::unix::fs::symlink(lib64, &link).ok()?,
    }
    Some(link)
}

/// A frame the renderer posted to a display (read back to host memory).
pub struct PostedFrame<'a> {
    pub display: u32,
    pub width: u32,
    pub height: u32,
    /// GL format of `pixels` (0x1908 RGBA, 0x80E1 BGRA).
    pub gl_format: u32,
    /// Rows are bottom-up when false.
    pub top_down: bool,
    pub pixels: &'a [u8],
}

type PostHandler = Box<dyn Fn(PostedFrame<'_>) + Send + Sync>;
static POST_HANDLER: OnceLock<PostHandler> = OnceLock::new();

#[allow(clippy::too_many_arguments)]
extern "C" fn on_post(
    _ctx: *mut c_void,
    display: u32,
    width: c_int,
    height: c_int,
    ydir: c_int,
    format: c_int,
    _ty: c_int,
    pixels: *mut u8,
) {
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else { return };
    if pixels.is_null() || w == 0 || h == 0 {
        return;
    }
    let Some(handler) = POST_HANDLER.get() else { return };
    // SAFETY: the renderer passes a w*h*4 readback buffer valid for the call.
    let px = unsafe { std::slice::from_raw_parts(pixels, w as usize * h as usize * 4) };
    handler(PostedFrame { display, width: w, height: h, gl_format: format as u32, top_down: ydir != -1, pixels: px });
}

type PostFn = extern "C" fn(*mut c_void, u32, c_int, c_int, c_int, c_int, c_int, *mut u8);
type GetFb = unsafe extern "C" fn() -> *mut c_void;
type SetPostCallback = unsafe extern "C" fn(*mut c_void, Option<PostFn>, *mut c_void, u32, bool);

impl Renderer {
    /// Receives every frame the renderer posts to displays 0–3 (host composition), read back
    /// asynchronously as BGRA. This is how the stock emulator streams its screen. Uses the
    /// renderer's exported C++ `FrameBuffer::getFB()` / `setPostCallback`.
    pub fn set_post_callback(&self, handler: PostHandler) -> Result<()> {
        // Registering starts the GL readback worker, and every post reads back through GL.
        if !self.has_gl() {
            return Err(Error::Call("FrameBuffer::setPostCallback (no GL backend)", 0));
        }
        let _ = POST_HANDLER.set(handler);
        let h = self.handle;
        let get_fb: GetFb = sym!(h, "_ZN9gfxstream4host11FrameBuffer5getFBEv");
        let set_cb: SetPostCallback = sym!(h, "_ZN9gfxstream4host11FrameBuffer15setPostCallbackEPFvPvjiiiiiPhES2_jb");
        // SAFETY: getFB returns the renderer's singleton (or null before init); setPostCallback
        // is a member function whose `this` is the first argument (Itanium C++ ABI).
        unsafe {
            let fb = get_fb();
            if fb.is_null() {
                return Err(Error::Call("FrameBuffer::getFB", 0));
            }
            // The guest composer may create its primary display with a non-zero host id.
            for display in 0..4 {
                set_cb(fb, Some(on_post as PostFn), std::ptr::null_mut(), display, true);
            }
        }
        Ok(())
    }
}

type OnLastColorBufferRef = unsafe extern "C" fn(*mut c_void, u32);

type GetLastPosted = unsafe extern "C" fn(*mut c_void) -> u32;
type FlushFromVk = unsafe extern "C" fn(*mut c_void, u32) -> bool;
type ReadColorBuffer = unsafe extern "C" fn(*mut c_void, u32, i32, i32, i32, i32, u32, *mut c_void, u64);

/// `GfxstreamFormat::R8G8B8A8_UNORM`.
const GFXSTREAM_RGBA8: u32 = 1;

/// Members whose signature has a `uint64_t`, which Itanium mangles as the type it is on the
/// platform: `unsigned long long` (`y`) on macOS, `unsigned long` (`m`) on LP64 Linux.
#[cfg(target_os = "macos")]
mod mangled {
    pub const READ_COLOR_BUFFER: &str = "_ZN9gfxstream4host11FrameBuffer15readColorBufferEjiiiiNS0_15GfxstreamFormatEPvy";
    pub const SET_DMA_GET_HOST_ADDR: &str = "_ZN9gfxstream4host31set_gfxstream_dma_get_host_addrEPFPvyE";
    pub const SET_DMA_UNLOCK: &str = "_ZN9gfxstream4host24set_gfxstream_dma_unlockEPFvyE";
}
#[cfg(target_os = "linux")]
mod mangled {
    pub const READ_COLOR_BUFFER: &str = "_ZN9gfxstream4host11FrameBuffer15readColorBufferEjiiiiNS0_15GfxstreamFormatEPvm";
    pub const SET_DMA_GET_HOST_ADDR: &str = "_ZN9gfxstream4host31set_gfxstream_dma_get_host_addrEPFPvmE";
    pub const SET_DMA_UNLOCK: &str = "_ZN9gfxstream4host24set_gfxstream_dma_unlockEPFvmE";
}

/// The renderer's `FrameBuffer` members simee calls directly.
#[derive(Clone, Copy)]
struct FbApi {
    get_fb: GetFb,
    last: GetLastPosted,
    flush: FlushFromVk,
    read: ReadColorBuffer,
    on_last: OnLastColorBufferRef,
}

const GET_FB: &str = "_ZN9gfxstream4host11FrameBuffer5getFBEv";
const GET_LAST_POSTED: &str = "_ZN9gfxstream4host11FrameBuffer24getLastPostedColorBufferEv";
const FLUSH_FROM_VK: &str = "_ZN9gfxstream4host11FrameBuffer22flushColorBufferFromVkEj";
const ON_LAST_REF: &str = "_ZN9gfxstream4host11FrameBuffer20onLastColorBufferRefEj";

impl FbApi {
    /// The symbols [`FbApi::load`] resolves.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    const SYMBOLS: [&str; 5] = [GET_FB, GET_LAST_POSTED, FLUSH_FROM_VK, mangled::READ_COLOR_BUFFER, ON_LAST_REF];

    fn load(h: *mut c_void) -> Result<FbApi> {
        Ok(FbApi {
            get_fb: sym!(h, @GET_FB),
            last: sym!(h, @GET_LAST_POSTED),
            flush: sym!(h, @FLUSH_FROM_VK),
            read: sym!(h, @mangled::READ_COLOR_BUFFER),
            on_last: sym!(h, @ON_LAST_REF),
        })
    }
}

/// Whether the renderer in `lib64` will run without GL when asked to, read from its file before
/// it's loaded: it exports the `FrameBuffer` members [`Renderer::can_read_last_posted`] needs and
/// has the direct Vulkan readback [`Renderer::can_read_vulkan_directly`] looks for. For choices
/// that must be made while the process is still single-threaded, before the renderer loads off
/// the boot path (the host ANGLE backend); the loaded image is checked again at init.
#[cfg(target_os = "macos")]
pub fn has_vulkan_readback(lib64: &Path) -> bool {
    let names: Vec<&str> = FbApi::SYMBOLS.into_iter().chain([HAS_EMULATION_VK, READ_COLOR_BUFFER_VK]).collect();
    macho::symbols(&lib64.join(BACKEND), &names).is_ok_and(|found| found.len() == names.len() && found.iter().all(Option::is_some))
}

impl Renderer {
    fn fb_api(&self) -> Result<FbApi> {
        let api = self.fb.get_or_init(|| {
            FbApi::load(self.handle).map_err(|e| match e {
                Error::Symbol(name) => name,
                _ => "FrameBuffer",
            })
        });
        api.map_err(Error::Symbol)
    }

    /// Whether [`Renderer::read_last_posted_into`] can work: the renderer exports the
    /// FrameBuffer members it uses.
    pub fn can_read_last_posted(&self) -> bool {
        self.fb_api().is_ok()
    }

    /// Whether frames can be read back from the Vulkan image without a GL backend, so the
    /// renderer may run without one. Checked before init. On macOS that takes the direct Vulkan
    /// readback ([`VkRead`]), which this build must have in the expected shape (the exported read
    /// aborts there without GL). On Linux the exported `FrameBuffer::readColorBuffer` reads the
    /// Vulkan image when there is no GL (SDK 37.3, `renderer_caps -vulkan-only`: byte-exact).
    pub fn can_read_vulkan_directly(&self) -> bool {
        self.can_read_last_posted() && (cfg!(target_os = "linux") || VkRead::symbols(self.handle, &self.backend).is_some())
    }

    /// The renderer has a GL backend: [`Renderer::set_post_callback`] works only then.
    pub fn has_gl(&self) -> bool {
        self.gl.load(Ordering::Relaxed)
    }

    /// The handle of the color buffer the renderer posted last (0 before any post). A different
    /// handle means a new post; a new post of the same buffer keeps it. Host composition posts
    /// its targets in turn (three on API 36), so each of its posts changes it. One load, no lock:
    /// cheap enough to poll every millisecond.
    pub fn last_posted_color_buffer(&self) -> Result<u32> {
        let f = self.fb_api()?;
        // SAFETY: getFB returns the renderer's singleton or null; getLastPostedColorBuffer is a
        // member function taking `this` first (Itanium ABI).
        unsafe {
            let fb = (f.get_fb)();
            if fb.is_null() {
                return Err(Error::Call("FrameBuffer::getFB", 0));
            }
            Ok((f.last)(fb))
        }
    }

    /// The display was told of a post of `width` x `height`: the buffer posted last is noted with
    /// that size, and returned. Call it where the post is noticed (the post callback, the post
    /// ticker's probe), so [`Renderer::last_posted_surface`] copies a buffer only at the size it
    /// was posted at.
    pub fn note_post(&self, width: u32, height: u32) -> Result<u32> {
        let cb = self.last_posted_color_buffer()?;
        self.post.note(cb, width, height);
        Ok(cb)
    }

    /// The last color buffer the renderer posted (host composition's output), read back as
    /// RGBA, `width` x `height`, top row first.
    pub fn read_last_posted(&self, width: u32, height: u32) -> Result<Vec<u8>> {
        let mut px = Vec::new();
        self.read_last_posted_into(width, height, &mut px)?;
        Ok(px)
    }

    /// [`Renderer::read_last_posted`] into `px`, reusing its allocation (one per capture, not
    /// one per frame).
    ///
    /// Composition runs in Vulkan. The exported path reads through GL: the Vulkan image is
    /// copied to host memory, uploaded into the GL texture, and read back again, under the
    /// FrameBuffer lock the guest's render threads need (the renderer's own readbacks, the post
    /// callback and `getScreenshot`, skip the sync and return stale pixels). The direct path
    /// ([`VkRead`]) reads the Vulkan image once, without that lock. It is used once its first
    /// frames that could show a difference (some red/blue difference, not vertically symmetric)
    /// match the exported path's byte for byte, and never if they don't (`EMU_READBACK=gl` turns
    /// it off).
    pub fn read_last_posted_into(&self, width: u32, height: u32, px: &mut Vec<u8>) -> Result<()> {
        px.resize(width as usize * height as usize * 4, 0);
        self.read_last_posted_to(width, height, px)
    }

    /// [`Renderer::read_last_posted_into`] into `px`, exactly `width * height * 4` bytes (a slot
    /// in shared memory).
    pub fn read_last_posted_to(&self, width: u32, height: u32, px: &mut [u8]) -> Result<()> {
        if px.len() != width as usize * height as usize * 4 {
            return Err(Error::Call("read_last_posted_to: buffer size", -22));
        }
        let f = self.fb_api()?;
        // SAFETY: getFB returns the renderer's singleton or null; getLastPostedColorBuffer is a
        // member function taking `this` first (Itanium ABI).
        let (fb, cb) = unsafe {
            let fb = (f.get_fb)();
            if fb.is_null() {
                return Err(Error::Call("FrameBuffer::getFB", 0));
            }
            (fb, (f.last)(fb))
        };
        if cb == 0 {
            return Err(Error::Call("FrameBuffer::getLastPostedColorBuffer", 0));
        }
        let vk = self.vk.get_or_init(|| VkRead::resolve(self.handle, &self.backend, fb));
        match (vk, VkTrust::from(self.vk_trust.load(Ordering::Relaxed))) {
            (Some(vk), VkTrust::Trusted) if vk.read(cb, width, height, px) => return Ok(()),
            (Some(vk), VkTrust::Unchecked) => {
                Self::read_through_gl(&f, fb, cb, width, height, px)?;
                // Only a frame where a channel swap or a flip would show tells the paths apart.
                if sim_core::pixels::reveals_layout(px, width, height) {
                    let mut direct = vec![0u8; px.len()];
                    let same = vk.read(cb, width, height, &mut direct) && *direct == *px;
                    self.vk_checked(same);
                }
                return Ok(());
            }
            _ => {}
        }
        if !self.has_gl() {
            if cfg!(target_os = "linux") {
                return Self::read_exported(&f, fb, cb, width, height, px);
            }
            return Err(Error::Call("read_last_posted_to: Vulkan readback (no GL backend)", 0));
        }
        Self::read_through_gl(&f, fb, cb, width, height, px)
    }

    /// Color buffer `cb` (`width` x `height`) read back as RGBA through the exported
    /// `FrameBuffer::readColorBuffer`, `px` exactly `width * height * 4` bytes: through GL when the
    /// renderer has it, else (Linux) straight from the Vulkan image. macOS without GL refuses:
    /// the exported read aborts in the renderer there.
    pub fn read_color_buffer(&self, cb: u32, width: u32, height: u32, px: &mut [u8]) -> Result<()> {
        if px.len() != width as usize * height as usize * 4 {
            return Err(Error::Call("read_color_buffer: buffer size", -22));
        }
        let f = self.fb_api()?;
        // SAFETY: getFB returns the renderer's singleton or null.
        let fb = unsafe { (f.get_fb)() };
        if fb.is_null() {
            return Err(Error::Call("FrameBuffer::getFB", 0));
        }
        match (self.has_gl(), cfg!(target_os = "linux")) {
            (true, _) => Self::read_through_gl(&f, fb, cb, width, height, px),
            (false, true) => Self::read_exported(&f, fb, cb, width, height, px),
            (false, false) => Err(Error::Call("read_color_buffer: no GL backend", 0)),
        }
    }

    /// `FrameBuffer::readColorBuffer` alone: without a GL backend the renderer reads the color
    /// buffer's Vulkan image (`ColorBuffer::readToBytes` falls through to it).
    fn read_exported(f: &FbApi, fb: *mut c_void, cb: u32, width: u32, height: u32, px: &mut [u8]) -> Result<()> {
        // SAFETY: a member function with `this` first (Itanium ABI) that writes at most the size
        // we pass, which is `px`'s length.
        unsafe { (f.read)(fb, cb, 0, 0, width as i32, height as i32, GFXSTREAM_RGBA8, px.as_mut_ptr().cast(), px.len() as u64) };
        Ok(())
    }

    fn read_through_gl(f: &FbApi, fb: *mut c_void, cb: u32, width: u32, height: u32, px: &mut [u8]) -> Result<()> {
        // SAFETY: member functions with `this` first (Itanium ABI); readColorBuffer writes at
        // most the size we pass, which is `px`'s length.
        unsafe {
            // GL holds stale pixels unless the Vulkan image made it there.
            if !(f.flush)(fb, cb) {
                return Err(Error::Call("FrameBuffer::flushColorBufferFromVk", 0));
            }
            (f.read)(fb, cb, 0, 0, width as i32, height as i32, GFXSTREAM_RGBA8, px.as_mut_ptr().cast(), px.len() as u64);
        }
        Ok(())
    }

    /// Records one comparison of the direct readback against the exported one: trusted after
    /// [`VK_CHECKS`] matches, off after as many mismatches (a frame that changed between the two
    /// reads can mismatch once; a different pixel layout always does).
    fn vk_checked(&self, same: bool) {
        static MATCHES: AtomicU8 = AtomicU8::new(0);
        static MISMATCHES: AtomicU8 = AtomicU8::new(0);
        let (count, then) = if same { (&MATCHES, VkTrust::Trusted) } else { (&MISMATCHES, VkTrust::Off) };
        // Decided once: a late comparison never flips a verdict.
        let decided = count.fetch_add(1, Ordering::Relaxed) + 1 >= VK_CHECKS
            && self.vk_trust.compare_exchange(VkTrust::Unchecked as u8, then as u8, Ordering::Relaxed, Ordering::Relaxed).is_ok();
        if decided && (VERBOSE.load(Ordering::Relaxed) || then == VkTrust::Off) {
            let what = if same { "direct Vulkan readback on" } else { "direct Vulkan readback differs from GL's; reading through GL" };
            eprintln!("emu: {what}");
        }
    }

    /// Frees color buffer `handle` once its last guest reference is gone (the guest's
    /// `refcount` pipe closed). Needs the RefCountPipe feature, under which consumers' explicit
    /// closes no longer free buffers.
    pub fn release_color_buffer(&self, handle: u32) -> Result<()> {
        let f = self.fb_api()?;
        // SAFETY: getFB returns the singleton or null; onLastColorBufferRef is a member function
        // taking `this` first (Itanium ABI) and tolerates unknown handles.
        unsafe {
            let fb = (f.get_fb)();
            if fb.is_null() {
                return Err(Error::Call("FrameBuffer::getFB", 0));
            }
            (f.on_last)(fb, handle);
        }
        Ok(())
    }
}

/// Comparisons a direct Vulkan readback needs before it's trusted (or turned off).
const VK_CHECKS: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum VkTrust {
    Unchecked = 0,
    Trusted = 1,
    Off = 2,
}

impl From<u8> for VkTrust {
    fn from(v: u8) -> Self {
        match v {
            0 => VkTrust::Unchecked,
            1 => VkTrust::Trusted,
            _ => VkTrust::Off,
        }
    }
}

/// `VkEmulation::readColorBufferToBytes(handle, x, y, w, h, out, out_size)`: reads a color
/// buffer's Vulkan image into `out` (one GPU copy into the renderer's staging buffer, one memcpy
/// out) under the Vulkan emulation's own lock, not the FrameBuffer's.
type ReadColorBufferVk = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32, u32, *mut c_void, u64) -> bool;

/// The renderer's direct Vulkan readback. gfxstream doesn't export it, so it is found the way a
/// debugger would: the function's address from the backend's symbol table (private symbols are
/// listed there, at unslid addresses, slid by the same amount as the exported `getFB`), and the
/// `VkEmulation` it's called on through the two loads of the exported
/// `FrameBuffer::hasEmulationVk()`, which is decoded and used only if it is exactly that
/// accessor ([`macho::pointer_test_offsets`]).
struct VkRead {
    emulation: *mut c_void,
    read: ReadColorBufferVk,
}

// SAFETY: `emulation` is the renderer's process-wide VkEmulation, which lives as long as the
// renderer and serializes its own calls with its mutex.
unsafe impl Send for VkRead {}
// SAFETY: as above.
unsafe impl Sync for VkRead {}

#[cfg(target_os = "macos")]
const HAS_EMULATION_VK: &str = "_ZNK9gfxstream4host11FrameBuffer14hasEmulationVkEv";
#[cfg(target_os = "macos")]
const READ_COLOR_BUFFER_VK: &str = "_ZN9gfxstream4host2vk11VkEmulation22readColorBufferToBytesEjjjjjPvy";

/// What [`VkRead::resolve`] finds in the loaded image before it needs a live FrameBuffer: the
/// image's slide, `hasEmulationVk`'s live address, and `readColorBufferToBytes`'s file address.
#[cfg_attr(target_os = "linux", allow(dead_code))]
struct VkSymbols {
    slide: u64,
    has_vk_live: u64,
    read: u64,
}

impl VkRead {
    /// The direct readback's symbols, from the renderer file that's loaded. The SDK's Linux
    /// backend is stripped of its symbol table, so there it's never found.
    #[cfg(target_os = "linux")]
    fn symbols(_handle: *mut c_void, _backend: &Path) -> Option<VkSymbols> {
        None
    }

    /// The direct readback's symbols, from the renderer file that's loaded.
    #[cfg(target_os = "macos")]
    fn symbols(handle: *mut c_void, backend: &Path) -> Option<VkSymbols> {
        if std::env::var("EMU_READBACK").is_ok_and(|v| v == "gl") {
            return None;
        }
        private_vk_member(handle, backend, READ_COLOR_BUFFER_VK)
    }

    #[cfg(target_os = "linux")]
    fn resolve(_handle: *mut c_void, _backend: &Path, _fb: *mut c_void) -> Option<VkRead> {
        None
    }

    #[cfg(target_os = "macos")]
    fn resolve(handle: *mut c_void, backend: &Path, fb: *mut c_void) -> Option<VkRead> {
        let VkSymbols { slide, has_vk_live, read } = Self::symbols(handle, backend)?;
        let emulation = live_vk_emulation(has_vk_live, fb)?;
        let read = read.wrapping_add(slide) as usize;
        // SAFETY: the address of `VkEmulation::readColorBufferToBytes(unsigned x5, void*,
        // unsigned long long)` in the loaded image (its symbol in the same build, checked by UUID,
        // slid like the image). The mangled name fixes the parameter types but not whether it's a
        // static member; gfxstream declares it an instance method (`this` first, Itanium ABI),
        // and a call that didn't match would fail the frame comparison before it's trusted.
        let read = unsafe { std::mem::transmute::<usize, ReadColorBufferVk>(read) };
        Some(VkRead { emulation, read })
    }

    /// Reads color buffer `cb` (`width` x `height`, its full extent) into `out`.
    fn read(&self, cb: u32, width: u32, height: u32, out: &mut [u8]) -> bool {
        // SAFETY: a live VkEmulation and its member function; it writes at most `out.len()` bytes
        // (it copies min(image bytes, size given) for packed formats; posted color buffers are
        // 4-byte RGBA, the size of `out`, and the first frames are checked against GL's).
        unsafe { (self.read)(self.emulation, cb, 0, 0, width, height, out.as_mut_ptr().cast(), out.len() as u64) }
    }
}

/// A private `VkEmulation` member's file address (as [`VkSymbols::read`]), with the image's slide
/// and `hasEmulationVk`'s live address, from the renderer file that's loaded.
#[cfg(target_os = "macos")]
fn private_vk_member(handle: *mut c_void, backend: &Path, member: &str) -> Option<VkSymbols> {
    // The file the symbols come from must be the image that's loaded (not, say, a renderer
    // updated in place since).
    let file_uuid = macho::image_uuid(&macho::load_commands(backend).ok()?)?;
    if loaded_uuid(handle)? != file_uuid {
        return None;
    }
    let found = macho::symbols(backend, &[GET_FB, HAS_EMULATION_VK, member]).ok()?;
    let [Some(get_fb), Some(has_vk), Some(read)] = found[..] else { return None };
    let live = |name: &str| {
        let c = CString::new(name).ok()?;
        // SAFETY: dlsym with a live handle and a NUL-terminated name.
        let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
        (!p.is_null()).then_some(p as u64)
    };
    // The image's slide, agreed on by two exported symbols.
    let slide = live(GET_FB)?.wrapping_sub(get_fb);
    let has_vk_live = live(HAS_EMULATION_VK)?;
    if has_vk_live.wrapping_sub(has_vk) != slide {
        return None;
    }
    Some(VkSymbols { slide, has_vk_live, read })
}

#[cfg(target_os = "macos")]
const COLOR_BUFFER_METAL: &str = "_ZN9gfxstream4host2vk11VkEmulation31getColorBufferMetalMemoryHandleEj";

/// `MTLResource_id VkEmulation::getColorBufferMetalMemoryHandle(uint32_t)`: the color buffer's
/// memory as gfxstream exported it (`vkGetMemoryMetalHandleEXT`, a placement `MTLHeap`),
/// unretained, null if it has none. It takes the emulation's own lock.
#[cfg(target_os = "macos")]
type ColorBufferMetal = unsafe extern "C" fn(*mut c_void, u32) -> *mut c_void;

/// The `-gpu metal` present path ([`metal`]) and whether its surfaces have matched the readback.
#[cfg(target_os = "macos")]
struct MetalPath {
    presenter: Mutex<metal::Presenter<metal::MetalGpu>>,
    /// [`VkTrust`], as a u8.
    trust: AtomicU8,
    matches: AtomicU8,
    mismatches: AtomicU8,
}

#[cfg(target_os = "macos")]
impl Renderer {
    /// Whether this renderer build has the Metal handle of its color buffers in the expected
    /// shape (the private `getColorBufferMetalMemoryHandle`, found as [`VkRead`] is), so
    /// [`Renderer::last_posted_surface`] can work.
    pub fn can_present_metal(&self) -> bool {
        private_vk_member(self.handle, &self.backend, COLOR_BUFFER_METAL).is_some()
    }

    /// The present path, set up once the renderer has its FrameBuffer.
    fn metal_path(&self) -> Option<&MetalPath> {
        if let Some(p) = self.metal.get() {
            return p.as_ref();
        }
        let f = self.fb_api().ok()?;
        // SAFETY: getFB returns the renderer's singleton or null (before init: not decided yet).
        let fb = unsafe { (f.get_fb)() };
        if fb.is_null() {
            return None;
        }
        self.metal
            .get_or_init(|| {
                let VkSymbols { slide, has_vk_live, read: member } = private_vk_member(self.handle, &self.backend, COLOR_BUFFER_METAL)?;
                let emulation = live_vk_emulation(has_vk_live, fb)? as usize;
                // SAFETY: the address of `VkEmulation::getColorBufferMetalMemoryHandle(unsigned)`
                // in the loaded image (its symbol in the same build, checked by UUID, slid like the
                // image); gfxstream declares it an instance method (`this` first, Itanium ABI), and
                // its disassembly (docs/research/metal-present.md) shows one: it locks the
                // emulation, finds the buffer, returns one pointer field.
                let lookup = unsafe { std::mem::transmute::<usize, ColorBufferMetal>(member.wrapping_add(slide) as usize) };
                // SAFETY: the live VkEmulation, which lives as long as the renderer, and the member
                // above, which tolerates unknown handles (null).
                let gpu = metal::MetalGpu::new(Box::new(move |cb| unsafe { lookup(emulation as *mut c_void, cb) }));
                Some(MetalPath {
                    presenter: Mutex::new(metal::Presenter::new(gpu)),
                    trust: AtomicU8::new(VkTrust::Unchecked as u8),
                    matches: AtomicU8::new(0),
                    mismatches: AtomicU8::new(0),
                })
            })
            .as_ref()
    }

    /// Color buffer `cb` (`width` x `height`) copied on the GPU into a pooled BGRA IOSurface
    /// ([`metal::Presenter::present`]), unchecked: for a buffer nothing is drawing into (the
    /// headless harness). `None` without the Metal path, a free surface, or memory to copy from.
    pub fn color_buffer_surface(&self, cb: u32, width: u32, height: u32) -> Option<Arc<metal::MetalSurface>> {
        let path = self.metal_path()?;
        path.presenter.lock().unwrap_or_else(|e| e.into_inner()).present(cb, width, height)
    }

    /// The last posted color buffer (host composition's output), `width` x `height`, copied on
    /// the GPU into a pooled BGRA IOSurface that stays this frame's until every holder drops it:
    /// the `-gpu metal` present path, ordered after the compositor's post (see [`metal`]).
    /// `Ok(None)` when there's nothing posted, no free surface, or the path is off: callers then
    /// read the frame back ([`Renderer::read_last_posted_to`]).
    ///
    /// It's used once its first frames that could show a difference match the readback of the
    /// same buffer byte for byte (red and blue swapped), and never if they don't: until then
    /// surfaces are made and checked but not handed out.
    ///
    /// The buffer is copied only if it is the post the display was last told of at `width` x
    /// `height` ([`Renderer::note_post`]): a post at another size that lands between the caller
    /// taking the frame's size and this call (a resize, an unfold) would otherwise be aliased at
    /// the old size, after trust, and go out as the wrong layout. That frame is read back instead.
    pub fn last_posted_surface(&self, width: u32, height: u32) -> Result<Option<Arc<metal::MetalSurface>>> {
        let live = self.last_posted_color_buffer()?;
        let Some(cb) = self.post.check(live, width, height) else { return Ok(None) };
        let Some(path) = self.metal_path() else { return Ok(None) };
        let trust = VkTrust::from(path.trust.load(Ordering::Relaxed));
        if trust == VkTrust::Off {
            return Ok(None);
        }
        let Some(surface) = path.presenter.lock().unwrap_or_else(|e| e.into_inner()).present(cb, width, height) else { return Ok(None) };
        if trust == VkTrust::Trusted {
            return Ok(Some(surface));
        }
        let mut want = vec![0u8; width as usize * height as usize * 4];
        let read = match self.vk.get().and_then(Option::as_ref) {
            Some(vk) => vk.read(cb, width, height, &mut want),
            None => self.read_color_buffer(cb, width, height, &mut want).is_ok(),
        };
        if read && sim_core::pixels::reveals_layout(&want, width, height) {
            let same = surface.with_bgra(|px, stride| metal::bgra_matches_rgba(px, stride, &want, width, height)) == Some(true);
            let (count, then) = if same { (&path.matches, VkTrust::Trusted) } else { (&path.mismatches, VkTrust::Off) };
            let decided = count.fetch_add(1, Ordering::Relaxed) + 1 >= VK_CHECKS
                && path.trust.compare_exchange(VkTrust::Unchecked as u8, then as u8, Ordering::Relaxed, Ordering::Relaxed).is_ok();
            if decided {
                let what = if same { "Metal present path on" } else { "Metal present path differs from the readback; reading frames back" };
                eprintln!("emu: {what}");
            }
        }
        Ok(None)
    }
}

/// The live `VkEmulation`, through the two loads of `FrameBuffer::hasEmulationVk()` (at
/// `has_vk_live`), decoded and used only if it is exactly that accessor.
#[cfg(target_os = "macos")]
fn live_vk_emulation(has_vk_live: u64, fb: *mut c_void) -> Option<*mut c_void> {
    // SAFETY: the address is the entry of an exported function in the loaded image, whose
    // code pages are mapped readable; 20 bytes stay inside its text segment.
    let code: [u32; 5] = unsafe { std::ptr::read_unaligned(has_vk_live as usize as *const [u32; 5]) };
    let (to_impl, to_vk) = macho::pointer_test_offsets(code)?;
    // SAFETY: the same two loads `hasEmulationVk()` does on the live FrameBuffer: its
    // implementation pointer, then that object's VkEmulation pointer (null without Vulkan).
    let emulation = unsafe {
        let imp = std::ptr::read((fb as usize + to_impl) as *const *mut c_void);
        if imp.is_null() {
            return None;
        }
        std::ptr::read((imp as usize + to_vk) as *const *mut c_void)
    };
    (!emulation.is_null()).then_some(emulation)
}

/// The `LC_UUID` of the image `handle` loaded, read from its Mach-O header in memory.
#[cfg(target_os = "macos")]
fn loaded_uuid(handle: *mut c_void) -> Option<[u8; 16]> {
    let c = CString::new(GET_FB).ok()?;
    // SAFETY: dlsym with a live handle and a NUL-terminated name.
    let sym = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if sym.is_null() {
        return None;
    }
    // SAFETY: an all-zero Dl_info is a valid out-parameter; dladdr fills it for an address inside
    // a loaded image.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `sym` is an address in a loaded image and `info` a live out-pointer.
    if unsafe { libc::dladdr(sym, &mut info) } == 0 || info.dli_fbase.is_null() {
        return None;
    }
    let base = info.dli_fbase.cast::<u8>().cast_const();
    // SAFETY: `dli_fbase` is the image's mach header, mapped readable with its load commands
    // right after it; 32 bytes of header first, then `sizeofcmds` (bounded) more.
    let image = unsafe {
        let header = std::slice::from_raw_parts(base, 32);
        let cmds = u32::from_le_bytes(header[20..24].try_into().ok()?) as usize;
        if cmds > 1 << 20 {
            return None;
        }
        std::slice::from_raw_parts(base, 32 + cmds)
    };
    macho::image_uuid(image)
}

/// Resolves a guest-physical address to the host address backing it (null if unmapped).
pub type DmaTranslator = Box<dyn Fn(u64) -> *mut u8 + Send + Sync>;
static DMA: std::sync::RwLock<Option<DmaTranslator>> = std::sync::RwLock::new(None);

extern "C" fn dma_get_host_addr(gpa: u64) -> *mut c_void {
    match DMA.read() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |f| f(gpa).cast()),
        Err(_) => std::ptr::null_mut(),
    }
}

extern "C" fn dma_unlock(_gpa: u64) {}

type SetDmaGetHostAddr = unsafe extern "C" fn(Option<extern "C" fn(u64) -> *mut c_void>);
type SetDmaUnlock = unsafe extern "C" fn(Option<extern "C" fn(u64)>);

impl Renderer {
    /// Installs how the renderer turns guest-physical addresses into host pointers. The guest's
    /// gralloc reads color buffers into goldfish address-space memory by physical address
    /// (`rcReadColorBufferDMA`), and direct-mapped GL buffers work the same way; the stock
    /// emulator registers these through the renderer's exported DMA hooks. Pass `None` before
    /// the memory the translator points into goes away.
    pub fn set_dma_translator(&self, translate: Option<DmaTranslator>) -> Result<()> {
        *DMA.write().unwrap_or_else(|e| e.into_inner()) = translate;
        let h = self.handle;
        let set_addr: SetDmaGetHostAddr = sym!(h, @mangled::SET_DMA_GET_HOST_ADDR);
        let set_unlock: SetDmaUnlock = sym!(h, @mangled::SET_DMA_UNLOCK);
        // SAFETY: both setters store a plain function pointer; ours stay valid for the process.
        unsafe {
            set_addr(Some(dma_get_host_addr));
            set_unlock(Some(dma_unlock));
        }
        Ok(())
    }
}

/// A renderer mapping widened to `len` bytes (whole guest pages). Renderer allocations are
/// page-aligned and page-granular, so the extra tail is part of the same allocation.
pub fn page_range(host: HostRange, len: usize) -> HostRange {
    // SAFETY: see above; the range starts at the renderer's page-aligned mapping.
    unsafe { HostRange::from_raw(host.as_ptr(), len.max(host.len())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ui_on_skia_gl_on_angle_runs_on_kosmickrisp_where_it_ships() {
        // API 34/35 phones: the guest's ANGLE asks for vertex inputs whose numeric type differs
        // from the shader's (a float format for a uvec2 input), which Metal refuses to compile
        // under MoltenVK and KosmicKrisp compiles (issue #412).
        assert_eq!(gpu_icd(true, true, None), Icd::KosmicKrisp);
        // A renderer revision without KosmicKrisp keeps MoltenVK.
        assert_eq!(gpu_icd(true, false, None), Icd::MoltenVk);
        // A UI on Skia Vulkan (API 36) or the GLES encoder (API 30 to 33) stays on MoltenVK.
        assert_eq!(gpu_icd(false, true, None), Icd::MoltenVk);
        assert_eq!(gpu_icd(false, false, None), Icd::MoltenVk);
    }

    #[test]
    fn android_emu_vk_icd_picks_the_gpu_driver_when_it_names_one() {
        assert_eq!(gpu_icd(true, true, Some("moltenvk")), Icd::MoltenVk);
        assert_eq!(gpu_icd(false, true, Some("kosmickrisp")), Icd::KosmicKrisp);
        // KosmicKrisp only where it ships; a software or unknown name leaves the choice to emu
        // (the `-gpu` mode picks software drivers).
        assert_eq!(gpu_icd(false, false, Some("kosmickrisp")), Icd::MoltenVk);
        assert_eq!(gpu_icd(true, true, Some("lavapipe")), Icd::KosmicKrisp);
        assert_eq!(gpu_icd(false, true, Some("bogus")), Icd::MoltenVk);
    }

    #[test]
    fn icd_names_round_trip() {
        for icd in [Icd::MoltenVk, Icd::KosmicKrisp, Icd::Lavapipe, Icd::SwiftShader] {
            assert_eq!(Icd::from_name(icd.name()), Some(icd));
        }
        assert_eq!(Icd::from_name("metal"), None);
    }

    #[test]
    fn the_environment_points_the_loader_at_the_chosen_icd() {
        let dir = std::env::temp_dir().join(format!("emu-gfx-env-{}", std::process::id()));
        let vk = dir.join("lib64/vulkan");
        std::fs::create_dir_all(&vk).expect("dir");
        for icd in [Icd::MoltenVk, Icd::KosmicKrisp] {
            std::fs::write(vk.join(icd.manifest()), "{}").expect("manifest");
        }
        let get = |icd: Icd, k: &str| environment(&dir, icd).into_iter().find(|(key, _)| *key == k).map(|(_, v)| v);
        let kk = vk.join("libkosmickrisp_icd.json").into_os_string();
        assert_eq!(get(Icd::KosmicKrisp, "VK_DRIVER_FILES"), Some(kk.clone()));
        assert_eq!(get(Icd::KosmicKrisp, "VK_ICD_FILENAMES"), Some(kk));
        assert_eq!(get(Icd::KosmicKrisp, "ANDROID_EMU_VK_ICD"), Some("kosmickrisp".into()));
        assert!(Icd::KosmicKrisp.ships_in(&dir.join("lib64")));
        assert!(!Icd::Lavapipe.ships_in(&dir.join("lib64")));
        // MoltenVK's settings are MoltenVK's alone; MoltenVK keeps them.
        let mvk = ["MVK_CONFIG_USE_METAL_ARGUMENT_BUFFERS", "MVK_CONFIG_VK_SEMAPHORE_SUPPORT_STYLE", "MVK_CONFIG_USE_MTLHEAP"];
        for k in mvk {
            assert_eq!(get(Icd::KosmicKrisp, k), None, "{k}");
        }
        let want = [
            ("MVK_CONFIG_USE_METAL_ARGUMENT_BUFFERS", "0"),
            ("MVK_CONFIG_VK_SEMAPHORE_SUPPORT_STYLE", "0"),
            ("MVK_CONFIG_USE_MTLHEAP", "1"),
        ];
        for (k, v) in want {
            assert_eq!(get(Icd::MoltenVk, k), Some(v.into()), "{k}");
        }
        assert_eq!(get(Icd::MoltenVk, "VK_DRIVER_FILES"), Some(vk.join("MoltenVK_icd.json").into_os_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The environment gfxstream ends up with after [`configure_environment`] for `icd` over the
    /// user's own `user`, and the notices it returned.
    fn seen(dir: &Path, icd: Icd, user: &[(&str, &str)]) -> (HashMap<String, OsString>, Vec<String>) {
        let current = |k: &str| user.iter().find(|(key, _)| *key == k).map(|(_, v)| OsString::from(v));
        let (set, notices) = applied(dir, icd, current);
        let mut env: HashMap<String, OsString> = user.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        env.extend(set.into_iter().map(|(k, v)| (k.to_string(), v)));
        (env, notices)
    }

    #[test]
    fn gfxstream_loads_the_driver_emu_chose_whatever_the_environment_named() {
        let dir = std::env::temp_dir().join(format!("emu-gfx-driver-{}", std::process::id()));
        let vk = dir.join("lib64/vulkan");
        std::fs::create_dir_all(&vk).expect("dir");
        let all = [Icd::MoltenVk, Icd::KosmicKrisp, Icd::Lavapipe, Icd::SwiftShader];
        for icd in all {
            std::fs::write(vk.join(icd.manifest()), "{}").expect("manifest");
        }
        let kk = vk.join(Icd::KosmicKrisp.manifest()).to_string_lossy().into_owned();
        let users: [&[(&str, &str)]; 4] = [
            &[],
            // scripts/bench/renderers.sh's KosmicKrisp arms.
            &[("ANDROID_EMU_VK_ICD", "kosmickrisp"), ("VK_DRIVER_FILES", &kk), ("VK_ICD_FILENAMES", &kk)],
            // A software driver's name under `-gpu host`, and a name that is no driver.
            &[("ANDROID_EMU_VK_ICD", "lavapipe")],
            &[("ANDROID_EMU_VK_ICD", "bogus")],
        ];
        for icd in all {
            for user in users {
                let (env, notices) = seen(&dir, icd, user);
                let manifest = vk.join(icd.manifest()).into_os_string();
                assert_eq!(env["ANDROID_EMU_VK_ICD"], icd.name(), "{icd:?} over {user:?}");
                assert_eq!(env["VK_DRIVER_FILES"], manifest, "{icd:?} over {user:?}");
                assert_eq!(env["VK_ICD_FILENAMES"], manifest, "{icd:?} over {user:?}");
                // A user's ANDROID_EMU_VK_ICD is honoured (it named emu's choice) or overridden,
                // and either way emu says so.
                match user.iter().find(|(k, _)| *k == "ANDROID_EMU_VK_ICD").map(|(_, v)| *v) {
                    None => assert!(notices.is_empty(), "{icd:?}: {notices:?}"),
                    Some(v) if v == icd.name() => {
                        assert!(notices.iter().any(|n| n.contains(&format!("ANDROID_EMU_VK_ICD={v} honoured"))), "{icd:?}: {notices:?}")
                    }
                    Some(v) => assert!(
                        notices.iter().any(|n| n.contains(&format!("ANDROID_EMU_VK_ICD={v} overridden")) && n.contains(icd.name())),
                        "{icd:?} over {v}: {notices:?}"
                    ),
                }
            }
        }
        // Settings that don't pick the driver stay the user's.
        let (env, notices) = seen(&dir, Icd::MoltenVk, &[("ANGLE_DEFAULT_PLATFORM", "swiftshader"), ("MVK_CONFIG_USE_MTLHEAP", "0")]);
        assert_eq!(env["ANGLE_DEFAULT_PLATFORM"], "swiftshader");
        assert_eq!(env["MVK_CONFIG_USE_MTLHEAP"], "0");
        assert!(notices.is_empty(), "{notices:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ranges(mem: &emu_hvf::GuestRam, n: usize) -> impl Iterator<Item = HostRange> + '_ {
        (0..n).map(move |i| mem.host_range(mem.base() + (i as u64 % 64) * 4096, 4096).expect("range"))
    }

    fn backing(mem: &emu_hvf::GuestRam, n: usize) -> Backing {
        let mut b = Backing::with_capacity(n);
        for r in ranges(mem, n) {
            b.push(r);
        }
        b
    }

    #[test]
    fn backing_is_the_ranges_as_iovecs() {
        let mem = emu_hvf::GuestRam::new(0x4000_0000, 64 * 4096).expect("ram");
        let b = backing(&mem, 3);
        let want = iovs(&ranges(&mem, 3).collect::<Vec<_>>());
        assert_eq!((b.len(), b.is_empty()), (3, false));
        for (a, w) in b.0.iter().zip(&want) {
            assert_eq!((a.base, a.len), (w.base, w.len));
        }
        assert!(Backing::default().is_empty());
    }

    #[test]
    fn iovec_arrays_live_until_their_use_ends() {
        let mem = emu_hvf::GuestRam::new(0x4000_0000, 64 * 4096).expect("ram");
        let arr = |n| backing(&mem, n).0.into_boxed_slice();
        let mut s = IovStore::default();
        assert!(s.attach(1, arr(4)).is_none(), "first attach replaces nothing");
        let first = s.attached[&1].as_ptr();
        let old = s.attach(1, arr(2)).expect("a re-attach hands back the replaced array");
        assert_eq!((old.as_ptr(), old.len(), s.entries()), (first, 4, 2), "the new one is kept");
        drop(old);
        s.blob(2, arr(5));
        s.blob(3, arr(0));
        assert_eq!(s.entries(), 7, "an empty blob array isn't kept");
        assert_eq!(s.detach(2).map(|v| v.len()), None, "a blob's array survives a detach");
        assert_eq!(s.entries(), 7);
        assert_eq!(s.detach(1).map(|v| v.len()), Some(2));
        assert!(s.attach(4, arr(3)).is_none());
        assert_eq!(s.attach(4, arr(0)).map(|v| v.len()), Some(3), "attaching nothing frees the last array");
        assert_eq!((s.release(2), s.release(2), s.entries()), (1, 0, 0), "unref frees the blob's array once");
        assert!(s.attach(5, arr(1)).is_none());
        s.blob(5, arr(1));
        assert_eq!(s.release(5), 2, "unref frees attached and blob arrays");
    }

    /// Building a 16K-entry backing list (a 64 MiB resource in 4 KiB pages): host ranges, then
    /// an iovec copy of them, leaked per call (before) vs the iovec array built in place and
    /// kept by the renderer until unref (now). Median of 9 runs.
    ///     cargo test --release -p simee-emu-gfxstream backing_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn backing_cost() {
        const N: usize = 16 * 1024;
        const REPS: u32 = 200;
        let mem = emu_hvf::GuestRam::new(0x4000_0000, 64 * 4096).expect("ram");
        let median = |f: &mut dyn FnMut()| {
            let mut runs: Vec<f64> = (0..9)
                .map(|_| {
                    let t = std::time::Instant::now();
                    for _ in 0..REPS {
                        f();
                    }
                    t.elapsed().as_secs_f64() * 1e6 / f64::from(REPS)
                })
                .collect();
            runs.sort_by(f64::total_cmp);
            runs[4]
        };
        let (mut old_us, mut new_us) = (Vec::new(), Vec::new());
        // Interleaved: the host is shared.
        for _ in 0..3 {
            old_us.push(median(&mut || {
                let hr: Vec<HostRange> = ranges(&mem, N).collect();
                let v = iovs(&hr).into_boxed_slice(); // was Box::leak'ed: 256 KiB per create
                std::hint::black_box(&v);
            }));
            new_us.push(median(&mut || {
                let v = backing(&mem, N).0.into_boxed_slice();
                std::hint::black_box(&v);
            }));
        }
        let mid = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v[1]
        };
        println!(
            "16K-entry backing: {:.1} us (ranges, then iovecs; leaked) -> {:.1} us (iovecs in place; freed on unref)",
            mid(&old_us),
            mid(&new_us)
        );
        println!("  runs before {old_us:.1?} after {new_us:.1?}; leaked before: {} KiB per create", N * std::mem::size_of::<Iov>() / 1024);
    }

    /// A library file named `name` in `dir` linking `deps` (written as the platform links them).
    fn write_lib(dir: &Path, name: &str, deps: &[&str]) {
        #[cfg(target_os = "macos")]
        let bytes = {
            let deps: Vec<String> =
                deps.iter().map(|d| if d.starts_with('/') || d.starts_with('@') { d.to_string() } else { format!("@rpath/{d}") }).collect();
            let deps: Vec<&str> = deps.iter().map(String::as_str).collect();
            macho::tests::fake_dylib(&format!("@rpath/{name}"), &deps)
        };
        #[cfg(target_os = "linux")]
        let bytes = elf::tests::fake_so(deps);
        std::fs::write(dir.join(name), bytes).expect("write");
    }

    fn file_names(paths: &[PathBuf]) -> Vec<String> {
        paths.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn bundled_deps_are_ordered_leaves_first() {
        let dir = std::env::temp_dir().join(format!("emu-gfxstream-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
        let lib = |n: &str| format!("lib{n}.{ext}");
        // backend -> common -> {metrics -> abseil, abseil}; backend -> abseil; a cycle back to
        // common; system and missing libraries are skipped; unrelated files aren't loaded.
        let system = if cfg!(target_os = "macos") { "/usr/lib/libc++.1.dylib" } else { "libc.so.6" };
        write_lib(&dir, BACKEND, &[&lib("common"), &lib("abseil"), system, &lib("missing")]);
        #[cfg(target_os = "macos")]
        write_lib(&dir, &lib("common"), &[&lib("metrics"), "@loader_path/libabseil.dylib"]);
        #[cfg(target_os = "linux")]
        write_lib(&dir, &lib("common"), &[&lib("metrics"), &lib("abseil")]);
        write_lib(&dir, &lib("metrics"), &[&lib("abseil"), &lib("common")]);
        write_lib(&dir, &lib("abseil"), &[]);
        write_lib(&dir, &lib("unrelated"), &[]);
        assert_eq!(file_names(&bundled_deps(&dir, BACKEND)), [lib("abseil"), lib("metrics"), lib("common")]);
        assert!(bundled_deps(&dir.join("nowhere"), BACKEND).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A renderer file with every FrameBuffer member and the direct readback's symbols runs
    /// without GL; one missing any of them (an unknown build) or no file at all keeps GL.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_vulkan_readback_is_read_from_the_file_before_it_loads() {
        let dir = std::env::temp_dir().join(format!("emu-gfxstream-readback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let all: Vec<&str> = FbApi::SYMBOLS.into_iter().chain([HAS_EMULATION_VK, READ_COLOR_BUFFER_VK]).collect();
        let write = |names: &[&str]| {
            // The readback's read is private in real builds (n_type 0x1e); the rest are exported.
            let syms: Vec<(&str, u8, u64)> = names
                .iter()
                .enumerate()
                .map(|(i, n)| (*n, if *n == READ_COLOR_BUFFER_VK { 0x1e } else { 0x0f }, 0x1000 * (i as u64 + 1)))
                .collect();
            std::fs::write(dir.join(BACKEND), macho::tests::fake_dylib_with_symbols(&syms)).expect("write");
        };
        write(&all);
        assert!(has_vulkan_readback(&dir));
        for missing in &all {
            let rest: Vec<&str> = all.iter().copied().filter(|n| n != missing).collect();
            write(&rest);
            assert!(!has_vulkan_readback(&dir), "without {missing}");
        }
        assert!(!has_vulkan_readback(&dir.join("nowhere")));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Libraries the backend opens by bare name come after its link-time dependencies, each after
    /// its own, with nothing loaded twice; a missing one is skipped.
    #[cfg(target_os = "linux")]
    #[test]
    fn preloads_add_the_dlopened_libraries_after_the_backends_deps() {
        let dir = std::env::temp_dir().join(format!("emu-gfxstream-preloads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        write_lib(&dir, BACKEND, &["libcommon.so", "libc.so.6"]);
        write_lib(&dir, "libcommon.so", &[]);
        assert_eq!(file_names(&preloads(&dir)), ["libcommon.so"], "no shader translator in lib64");
        write_lib(&dir, "libshadertranslator.so", &["libcommon.so", "libglsl.so"]);
        write_lib(&dir, "libglsl.so", &[]);
        assert_eq!(file_names(&preloads(&dir)), ["libcommon.so", "libglsl.so", "libshadertranslator.so"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
