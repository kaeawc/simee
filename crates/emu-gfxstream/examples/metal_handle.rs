//! Kill gate for a Metal present path (#356, docs/research/metal-present.md): no guest, no VM.
//! Initializes the renderer as `emu` does for an API 36 guest (Vulkan only), creates color
//! buffers through the stream_renderer API, uploads a known pattern, and then:
//!
//! - reads each one back with the renderer's private direct Vulkan readback
//!   (`VkEmulation::readColorBufferToBytes`, what emu streams frames with), checks the bytes and
//!   times it;
//! - asks the renderer for the buffer's Metal handle (`VkEmulation::getColorBufferMetalMemoryHandle`,
//!   also private) and inspects it with the Objective-C runtime: class, protocols, IOSurface;
//! - when it is a placement `MTLHeap`, aliases the color buffer's image with an `MTLTexture` at
//!   offset 0, copies that into an IOSurface-backed texture (a blit to RGBA, and a compute pass
//!   to BGRA, the encoder's format), checks the bytes against the readback and times both.
//!
//! ```text
//! cargo run --release -p simee-emu-gfxstream --example metal_handle -- <lib64> [moltenvk|kosmickrisp] [iterations]
//! ```
//!
//! Private symbols are found as the library finds `VkRead`: from the backend file's symbol
//! table, only when the file's `LC_UUID` is the loaded image's, slid by the slide two exported
//! symbols agree on. The copy here runs on its own Metal queue with no ordering against the
//! renderer's: fine for buffers nothing is drawing into, not for a live guest.
#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    imp::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("metal_handle runs on macOS only");
}

#[cfg(target_os = "macos")]
mod imp {
    use simee_emu_gfxstream as gfx;
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    type Res<T> = Result<T, Box<dyn std::error::Error>>;
    type Id = *mut c_void;
    type Sel = *const c_void;

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_msgSend();
        fn objc_getClass(name: *const c_char) -> Id;
        fn objc_getProtocol(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn object_getClassName(obj: Id) -> *const c_char;
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
        fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
        fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    }

    const GET_FB: &str = "_ZN9gfxstream4host11FrameBuffer5getFBEv";
    const HAS_EMULATION_VK: &str = "_ZNK9gfxstream4host11FrameBuffer14hasEmulationVkEv";
    const READ_COLOR_BUFFER_VK: &str = "_ZN9gfxstream4host2vk11VkEmulation22readColorBufferToBytesEjjjjjPvy";
    const COLOR_BUFFER_METAL: &str = "_ZN9gfxstream4host2vk11VkEmulation31getColorBufferMetalMemoryHandleEj";

    /// `VkEmulation::readColorBufferToBytes(handle, x, y, w, h, out, out_size)`.
    type ReadVk = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32, u32, *mut c_void, u64) -> bool;
    /// `MTLResource_id VkEmulation::getColorBufferMetalMemoryHandle(uint32_t)`: the color buffer's
    /// memory as gfxstream exported it (`vkGetMemoryMetalHandleEXT`), unretained, null if none.
    type MetalHandle = unsafe extern "C" fn(*mut c_void, u32) -> Id;

    const MTL_PIXEL_FORMAT_RGBA8: u64 = 70;
    const MTL_PIXEL_FORMAT_BGRA8: u64 = 80;
    const USAGE_SHADER_READ: u64 = 1;
    const USAGE_SHADER_WRITE: u64 = 2;
    const USAGE_RENDER_TARGET: u64 = 4;
    const USAGE_PIXEL_FORMAT_VIEW: u64 = 0x10;
    const STORAGE_SHARED: u64 = 0;
    const HEAP_PLACEMENT: u64 = 1;
    const COMMAND_BUFFER_COMPLETED: u64 = 4;

    const KERNEL: &str = "#include <metal_stdlib>\nusing namespace metal;\nkernel void to_bgra(texture2d<float, access::read> src [[texture(0)]], texture2d<float, access::write> dst [[texture(1)]], uint2 gid [[thread_position_in_grid]]) { if (gid.x < dst.get_width() && gid.y < dst.get_height()) dst.write(src.read(gid), gid); }\n";

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MtlSize {
        width: u64,
        height: u64,
        depth: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SizeAndAlign {
        size: u64,
        align: u64,
    }

    fn sel(name: &str) -> Sel {
        let c = CString::new(name).unwrap_or_default();
        // SAFETY: a NUL-terminated selector name; the runtime interns it.
        unsafe { sel_registerName(c.as_ptr()) }
    }

    fn class(name: &str) -> Id {
        let c = CString::new(name).unwrap_or_default();
        // SAFETY: a NUL-terminated class name; null when no loaded image defines it.
        unsafe { objc_getClass(c.as_ptr()) }
    }

    fn class_name(obj: Id) -> String {
        if obj.is_null() {
            return "nil".into();
        }
        // SAFETY: `obj` is a live Objective-C object; the name is a static C string.
        unsafe { CStr::from_ptr(object_getClassName(obj)).to_string_lossy().into_owned() }
    }

    fn responds(obj: Id, s: Sel) -> bool {
        if obj.is_null() {
            return false;
        }
        // SAFETY: every NSObject implements `-respondsToSelector:(SEL) -> BOOL`.
        let f = unsafe { std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(Id, Sel, Sel) -> bool>(objc_msgSend) };
        // SAFETY: `obj` is a live object (checked non-null above, handed out by Metal or the runtime).
        unsafe { f(obj, sel("respondsToSelector:"), s) }
    }

    /// `[obj sel args...]` typed as `fn(args...) -> ret`, after `respondsToSelector:` says yes.
    macro_rules! send {
        ($obj:expr, $sel:expr $(, $arg:expr => $ty:ty)* ; $ret:ty) => {{
            let obj: Id = $obj;
            let s = sel($sel);
            if responds(obj, s) {
                // SAFETY: the method's signature from the Metal / Foundation headers; arm64
                // `objc_msgSend` takes the receiver, the selector and then the C arguments.
                let f = unsafe { std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret>(objc_msgSend) };
                // SAFETY: `obj` is live and responds to the selector; arguments match the signature.
                Ok::<$ret, String>(unsafe { f(obj, s $(, $arg)*) })
            } else {
                Err(format!("{} does not respond to {}", class_name(obj), $sel))
            }
        }};
    }

    fn describe(obj: Id) -> String {
        let Ok(text) = send!(obj, "description"; Id) else { return class_name(obj) };
        match send!(text, "UTF8String"; *const c_char) {
            // SAFETY: a NUL-terminated string owned by the (autoreleased) NSString, copied at once.
            Ok(p) if !p.is_null() => unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() },
            _ => class_name(obj),
        }
    }

    fn conforms(obj: Id, protocol: &str) -> bool {
        let c = CString::new(protocol).unwrap_or_default();
        // SAFETY: a NUL-terminated protocol name; null when no loaded image declares it.
        let p = unsafe { objc_getProtocol(c.as_ptr()) };
        !p.is_null() && send!(obj, "conformsToProtocol:", p => Id; bool).unwrap_or(false)
    }

    fn nsstring(s: &str) -> Res<Id> {
        let c = CString::new(s)?;
        let obj = send!(class("NSString"), "stringWithUTF8String:", c.as_ptr() => *const c_char; Id)?;
        if obj.is_null() { Err("NSString".into()) } else { Ok(obj) }
    }

    /// Runs `f` inside an autorelease pool (command buffers and encoders are autoreleased).
    fn pooled<T>(f: impl FnOnce() -> T) -> T {
        // SAFETY: push and pop are balanced on this thread around `f`.
        let pool = unsafe { objc_autoreleasePoolPush() };
        let out = f();
        // SAFETY: the pool pushed above, popped once.
        unsafe { objc_autoreleasePoolPop(pool) };
        out
    }

    fn thread_cpu_ms() -> f64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a valid clock id and a live out-pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6
    }

    /// User + system CPU of the whole process (every thread: the driver's included), in ms.
    fn process_cpu_ms() -> f64 {
        // SAFETY: an all-zero rusage is a valid out-parameter.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: a live out-pointer.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        let ms = |t: libc::timeval| t.tv_sec as f64 * 1e3 + f64::from(t.tv_usec) / 1e3;
        ms(ru.ru_utime) + ms(ru.ru_stime)
    }

    fn median(v: &mut [f64]) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    // --- Mach-O: just enough to read private symbols, as `macho.rs` does inside the library.

    fn u32_le(b: &[u8], off: usize) -> Option<u32> {
        Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
    }

    /// The arm64 image's start in a thin or universal file.
    fn arm64_slice(file: &[u8]) -> Option<usize> {
        let be = |off: usize| Some(u32::from_be_bytes(file.get(off..off + 4)?.try_into().ok()?));
        if be(0)? == 0xcafe_babe {
            (0..be(4)? as usize).find_map(|i| (be(8 + i * 20)? == 0x0100_000c).then(|| be(8 + i * 20 + 8).map(|o| o as usize)).flatten())
        } else {
            (u32_le(file, 0)? == 0xfeed_facf).then_some(0)
        }
    }

    /// The `LC_UUID` of a Mach-O header plus its load commands.
    fn image_uuid(image: &[u8]) -> Option<[u8; 16]> {
        let mut off = 32;
        for _ in 0..u32_le(image, 16)? {
            let (cmd, size) = (u32_le(image, off)?, u32_le(image, off + 4)? as usize);
            if cmd == 0x1b {
                return image.get(off + 8..off + 24)?.try_into().ok();
            }
            off += size.max(8);
        }
        None
    }

    /// The UUID and the unslid addresses of `names` (C++ mangled, without the leading `_`) in the
    /// file's symbol table, private symbols included.
    fn file_symbols(path: &Path, names: &[&str]) -> Option<([u8; 16], Vec<Option<u64>>)> {
        let file = std::fs::read(path).ok()?;
        let image = file.get(arm64_slice(&file)?..)?;
        let uuid = image_uuid(image)?;
        let mut off = 32;
        let mut symtab = None;
        for _ in 0..u32_le(image, 16)? {
            let (cmd, size) = (u32_le(image, off)?, u32_le(image, off + 4)? as usize);
            if cmd == 0x2 {
                let at = |i| u32_le(image, off + i).map(|v| v as usize);
                symtab = Some((at(8)?, at(12)?, at(16)?));
            }
            off += size.max(8);
        }
        let (symoff, nsyms, stroff) = symtab?;
        let mut found = vec![None; names.len()];
        for i in 0..nsyms {
            let e = image.get(symoff + i * 16..symoff + i * 16 + 16)?;
            // Section-defined, not a debugger entry.
            if e[4] & 0xe0 != 0 || e[4] & 0x0e != 0x0e {
                continue;
            }
            let Some(raw) = image.get(stroff + u32_le(e, 0)? as usize..) else { continue };
            let name = &raw[..raw.iter().position(|&c| c == 0).unwrap_or(raw.len())];
            if let Some(k) = name.strip_prefix(b"_").and_then(|n| names.iter().position(|w| w.as_bytes() == n)) {
                found[k] = Some(u64::from_le_bytes(e[8..16].try_into().ok()?));
            }
        }
        Some((uuid, found))
    }

    /// The UUID of the loaded image that defines `addr`, from its Mach-O header in memory.
    fn loaded_uuid(addr: *mut c_void) -> Option<[u8; 16]> {
        // SAFETY: an all-zero Dl_info is a valid out-parameter.
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // SAFETY: `addr` is inside a loaded image; `info` is a live out-pointer.
        if unsafe { libc::dladdr(addr, &mut info) } == 0 || info.dli_fbase.is_null() {
            return None;
        }
        let base = info.dli_fbase.cast::<u8>().cast_const();
        // SAFETY: the image's mach header (32 bytes) is mapped readable, its load commands
        // (`sizeofcmds`, bounded here) right after it.
        let image = unsafe {
            let cmds = u32_le(std::slice::from_raw_parts(base, 32), 20)? as usize;
            if cmds > 1 << 20 {
                return None;
            }
            std::slice::from_raw_parts(base, 32 + cmds)
        };
        image_uuid(image)
    }

    /// `ldr xA, [x0, #a]; ldr xB, [xA, #b]; cmp xB, #0; cset w0, ne; ret`: `(a, b)`.
    fn pointer_test_offsets(code: [u32; 5]) -> Option<(usize, usize)> {
        let ldr = |w: u32| (w & 0xffc0_0000 == 0xf940_0000).then(|| (((w >> 10) & 0xfff) as usize * 8, (w >> 5) & 31, w & 31));
        let (a, base, ra) = ldr(code[0])?;
        let (b, mid, rb) = ldr(code[1])?;
        let cmp_zero = 0xf100_001f | (rb << 5);
        (base == 0 && mid == ra && code[2] == cmp_zero && code[3] == 0x1a9f_07e0 && code[4] == 0xd65f_03c0).then_some((a, b))
    }

    struct Private {
        emulation: *mut c_void,
        read: ReadVk,
        metal: MetalHandle,
    }

    /// Resolves the two private VkEmulation members and the live VkEmulation, or says why not.
    fn resolve(backend: &Path) -> Res<Private> {
        let path = CString::new(backend.as_os_str().as_encoded_bytes())?;
        // SAFETY: a NUL-terminated path; RTLD_NOLOAD returns the image `Renderer::load` opened.
        let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
        if h.is_null() {
            return Err("backend is not loaded".into());
        }
        let live = |name: &str| -> Option<*mut c_void> {
            let c = CString::new(name).ok()?;
            // SAFETY: dlsym with a live handle and a NUL-terminated name.
            let p = unsafe { libc::dlsym(h, c.as_ptr()) };
            (!p.is_null()).then_some(p)
        };
        let get_fb_live = live(GET_FB).ok_or("getFB not exported")?;
        let has_vk_live = live(HAS_EMULATION_VK).ok_or("hasEmulationVk not exported")?;
        let (uuid, found) =
            file_symbols(backend, &[GET_FB, HAS_EMULATION_VK, READ_COLOR_BUFFER_VK, COLOR_BUFFER_METAL]).ok_or("no symbol table")?;
        if loaded_uuid(get_fb_live) != Some(uuid) {
            return Err("the backend file is not the loaded image (UUID differs)".into());
        }
        let [Some(get_fb), Some(has_vk), Some(read), Some(metal)] = found[..] else {
            return Err(format!("symbols missing from this build: {found:?}").into());
        };
        let slide = (get_fb_live as u64).wrapping_sub(get_fb);
        if (has_vk_live as u64).wrapping_sub(has_vk) != slide {
            return Err("exported symbols disagree on the slide".into());
        }
        // SAFETY: an exported function's entry in the loaded image; 20 bytes stay in its text.
        let code: [u32; 5] = unsafe { std::ptr::read_unaligned(has_vk_live as *const [u32; 5]) };
        let (to_impl, to_vk) = pointer_test_offsets(code).ok_or("hasEmulationVk is not the expected accessor")?;
        // SAFETY: `FrameBuffer::getFB()` (exported, no arguments) returns the singleton or null.
        let fb = unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> *mut c_void>(get_fb_live)() };
        if fb.is_null() {
            return Err("no FrameBuffer".into());
        }
        // SAFETY: the two loads `hasEmulationVk()` itself makes on the live FrameBuffer.
        let emulation = unsafe {
            let imp = std::ptr::read((fb as usize + to_impl) as *const *mut c_void);
            if imp.is_null() {
                return Err("FrameBuffer has no impl".into());
            }
            std::ptr::read((imp as usize + to_vk) as *const *mut c_void)
        };
        if emulation.is_null() {
            return Err("no VkEmulation (Vulkan is off)".into());
        }
        // SAFETY: addresses of the two members in the loaded image (same build by UUID, slid like
        // it); their mangled names fix the parameters, gfxstream declares both instance methods.
        let (read, metal) = unsafe {
            (
                std::mem::transmute::<usize, ReadVk>(read.wrapping_add(slide) as usize),
                std::mem::transmute::<usize, MetalHandle>(metal.wrapping_add(slide) as usize),
            )
        };
        Ok(Private { emulation, read, metal })
    }

    /// A color buffer created and filled through the stream_renderer API, with its backing.
    struct ColorBuffer {
        id: u32,
        width: u32,
        height: u32,
        src: Vec<u8>,
    }

    fn create(r: &gfx::Renderer, id: u32, width: u32, height: u32) -> Res<ColorBuffer> {
        let mut src: Vec<u8> = (0..width * height * 4).map(|i| (i.wrapping_mul(7).wrapping_add(id) % 251) as u8).collect();
        // SAFETY: `src` lives in the returned ColorBuffer, which outlives every use of the resource
        // (the process exits with both).
        let backing = unsafe { gfx::HostRange::from_raw(src.as_mut_ptr(), src.len()) };
        // PIPE_TEXTURE_2D, R8G8B8A8 (virgl format 67), render target | sampler view: a color buffer.
        let args = gfx::ResourceCreateArgs {
            handle: id,
            target: 2,
            format: 67,
            bind: (1 << 1) | (1 << 3),
            width,
            height,
            depth: 1,
            array_size: 1,
            ..Default::default()
        };
        r.resource_create(args, &[backing])?;
        r.transfer_write(id, 0, 0, width * 4, 0, gfx::Box3 { x: 0, y: 0, z: 0, w: width, h: height, d: 1 }, 0)?;
        Ok(ColorBuffer { id, width, height, src })
    }

    struct Stats {
        wall: f64,
        thread_cpu: f64,
        process_cpu: f64,
        gpu: f64,
    }

    impl std::fmt::Display for Stats {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wall {:.3} ms, this thread {:.3} ms CPU, process {:.3} ms CPU", self.wall, self.thread_cpu, self.process_cpu)?;
            if self.gpu > 0.0 {
                write!(f, ", GPU {:.3} ms", self.gpu)?;
            }
            Ok(())
        }
    }

    fn vk_read(p: &Private, cb: &ColorBuffer, out: &mut [u8]) -> bool {
        // SAFETY: a live VkEmulation and its member; it writes at most `out.len()` bytes (the
        // buffer's full RGBA extent).
        unsafe { (p.read)(p.emulation, cb.id, 0, 0, cb.width, cb.height, out.as_mut_ptr().cast(), out.len() as u64) }
    }

    fn time_vk_read(p: &Private, cb: &ColorBuffer, n: usize) -> Res<Stats> {
        let mut out = vec![0u8; cb.src.len()];
        let (mut wall, mut cpu) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let p0 = process_cpu_ms();
        for _ in 0..n {
            let (t, c) = (Instant::now(), thread_cpu_ms());
            if !vk_read(p, cb, &mut out) {
                return Err("readColorBufferToBytes failed".into());
            }
            wall.push(t.elapsed().as_secs_f64() * 1e3);
            cpu.push(thread_cpu_ms() - c);
        }
        let process_cpu = (process_cpu_ms() - p0) / n as f64;
        Ok(Stats { wall: median(&mut wall), thread_cpu: median(&mut cpu), process_cpu, gpu: 0.0 })
    }

    /// An IOSurface-backed texture: the surface, its texture, and its stride.
    struct Surface {
        surface: *mut c_void,
        texture: Id,
        stride: usize,
    }

    fn cf_number(v: i64) -> *const c_void {
        // SAFETY: kCFNumberSInt64Type (4) reads an i64 from a live pointer.
        unsafe { CFNumberCreate(std::ptr::null(), 4, (&raw const v).cast()) }
    }

    fn surface(device: Id, width: u32, height: u32, format: u64) -> Res<Surface> {
        let align = send!(device, "minimumLinearTextureAlignmentForPixelFormat:", format => u64; u64)?.max(16) as usize;
        let stride = (width as usize * 4).next_multiple_of(align);
        let fourcc = u32::from_be_bytes(if format == MTL_PIXEL_FORMAT_BGRA8 { *b"BGRA" } else { *b"RGBA" });
        // SAFETY: a CF dictionary with CF-type callbacks; keys are IOSurface's constant CFStrings,
        // values fresh CFNumbers the dictionary retains (released here after insertion).
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
                (kIOSurfacePixelFormat, i64::from(fourcc)),
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
            return Err("IOSurfaceCreate".into());
        }
        let desc = send!(class("MTLTextureDescriptor"), "texture2DDescriptorWithPixelFormat:width:height:mipmapped:",
            format => u64, u64::from(width) => u64, u64::from(height) => u64, false => bool; Id)?;
        send!(desc, "setStorageMode:", STORAGE_SHARED => u64; ())?;
        send!(desc, "setUsage:", USAGE_SHADER_READ | USAGE_SHADER_WRITE => u64; ())?;
        let texture = send!(device, "newTextureWithDescriptor:iosurface:plane:", desc => Id, surface => *mut c_void, 0 => u64; Id)?;
        if texture.is_null() {
            return Err("newTextureWithDescriptor:iosurface:plane: returned nil".into());
        }
        // SAFETY: a live IOSurface.
        let stride = unsafe { IOSurfaceGetBytesPerRow(surface) };
        Ok(Surface { surface, texture, stride })
    }

    /// Compares the surface's rows with `want` (RGBA, tightly packed); `bgra` swaps red and blue.
    fn surface_matches(s: &Surface, want: &[u8], width: u32, height: u32, bgra: bool) -> Res<bool> {
        let row = width as usize * 4;
        // SAFETY: a live IOSurface, locked read-only (1) and unlocked below; while locked its
        // `height` rows of `stride` bytes are mapped at the base address.
        unsafe {
            if IOSurfaceLock(s.surface, 1, std::ptr::null_mut()) != 0 {
                return Err("IOSurfaceLock".into());
            }
            let base = IOSurfaceGetBaseAddress(s.surface).cast::<u8>();
            let px = std::slice::from_raw_parts(base, s.stride * (height as usize - 1) + row);
            let same = (0..height as usize).all(|y| {
                let (got, w) = (&px[y * s.stride..y * s.stride + row], &want[y * row..y * row + row]);
                if bgra {
                    got.as_chunks::<4>()
                        .0
                        .iter()
                        .zip(w.as_chunks::<4>().0)
                        .all(|(g, w)| g[0] == w[2] && g[1] == w[1] && g[2] == w[0] && g[3] == w[3])
                } else {
                    got == w
                }
            });
            IOSurfaceUnlock(s.surface, 1, std::ptr::null_mut());
            Ok(same)
        }
    }

    /// What the Metal handle is.
    fn inspect(handle: Id) -> String {
        let protocols: Vec<&str> =
            ["MTLResource", "MTLHeap", "MTLTexture", "MTLBuffer", "MTLAllocation"].into_iter().filter(|p| conforms(handle, p)).collect();
        let mut out = format!("class {}, conforms to {}", class_name(handle), protocols.join(" "));
        let num = |s: &str| send!(handle, s; u64).map_or_else(|_| "-".to_string(), |v| v.to_string());
        if protocols.contains(&"MTLHeap") {
            out += &format!(
                "; heap type {}, size {}, usedSize {}, storageMode {}, cpuCacheMode {}, hazardTrackingMode {}",
                num("type"),
                num("size"),
                num("usedSize"),
                num("storageMode"),
                num("cpuCacheMode"),
                num("hazardTrackingMode")
            );
        }
        if protocols.contains(&"MTLTexture") {
            let surface = send!(handle, "iosurface"; *mut c_void).unwrap_or(std::ptr::null_mut());
            out += &format!("; texture {}x{} format {}, iosurface {:?}", num("width"), num("height"), num("pixelFormat"), surface);
        }
        if protocols.contains(&"MTLBuffer") {
            out += &format!("; buffer length {}", num("length"));
        }
        out
    }

    /// A texture over the color buffer's image: the heap's memory at offset 0, described as the
    /// image is (RGBA8, its size, private, `usage`).
    fn alias(heap: Id, cb: &ColorBuffer, usage: u64) -> Res<Id> {
        let device = send!(heap, "device"; Id)?;
        let desc = send!(class("MTLTextureDescriptor"), "texture2DDescriptorWithPixelFormat:width:height:mipmapped:",
            MTL_PIXEL_FORMAT_RGBA8 => u64, u64::from(cb.width) => u64, u64::from(cb.height) => u64, false => bool; Id)?;
        let (storage, cache, hazard) =
            (send!(heap, "storageMode"; u64)?, send!(heap, "cpuCacheMode"; u64)?, send!(heap, "hazardTrackingMode"; u64)?);
        send!(desc, "setStorageMode:", storage => u64; ())?;
        send!(desc, "setCpuCacheMode:", cache => u64; ())?;
        send!(desc, "setHazardTrackingMode:", hazard => u64; ())?;
        send!(desc, "setUsage:", usage => u64; ())?;
        let need = send!(device, "heapTextureSizeAndAlignWithDescriptor:", desc => Id; SizeAndAlign)?;
        let size = send!(heap, "size"; u64)?;
        if need.size > size {
            return Err(format!("the texture needs {} bytes, the heap has {size}", need.size).into());
        }
        let t = send!(heap, "newTextureWithDescriptor:offset:", desc => Id, 0 => u64; Id)?;
        if t.is_null() { Err("newTextureWithDescriptor:offset: returned nil".into()) } else { Ok(t) }
    }

    enum Pass {
        Blit,
        Compute(Id),
    }

    /// One copy of `src` into `dst` on `queue`: (CPU ms to encode and commit, wall ms to done,
    /// GPU ms).
    fn copy_once(queue: Id, how: &Pass, src: Id, dst: Id, width: u32, height: u32) -> Res<(f64, f64, f64)> {
        pooled(|| {
            let t = Instant::now();
            let cmd = send!(queue, "commandBuffer"; Id)?;
            match how {
                Pass::Blit => {
                    let enc = send!(cmd, "blitCommandEncoder"; Id)?;
                    send!(enc, "copyFromTexture:toTexture:", src => Id, dst => Id; ())?;
                    send!(enc, "endEncoding"; ())?;
                }
                Pass::Compute(pipeline) => {
                    let enc = send!(cmd, "computeCommandEncoder"; Id)?;
                    send!(enc, "setComputePipelineState:", *pipeline => Id; ())?;
                    send!(enc, "setTexture:atIndex:", src => Id, 0 => u64; ())?;
                    send!(enc, "setTexture:atIndex:", dst => Id, 1 => u64; ())?;
                    let grid = MtlSize { width: u64::from(width), height: u64::from(height), depth: 1 };
                    let group = MtlSize { width: 16, height: 16, depth: 1 };
                    send!(enc, "dispatchThreads:threadsPerThreadgroup:", grid => MtlSize, group => MtlSize; ())?;
                    send!(enc, "endEncoding"; ())?;
                }
            }
            send!(cmd, "commit"; ())?;
            let committed = t.elapsed().as_secs_f64() * 1e3;
            send!(cmd, "waitUntilCompleted"; ())?;
            let done = t.elapsed().as_secs_f64() * 1e3;
            if send!(cmd, "status"; u64)? != COMMAND_BUFFER_COMPLETED {
                return Err(format!("command buffer failed: {}", describe(send!(cmd, "error"; Id)?)).into());
            }
            let gpu = (send!(cmd, "GPUEndTime"; f64)? - send!(cmd, "GPUStartTime"; f64)?) * 1e3;
            Ok((committed, done, gpu))
        })
    }

    fn time_copy(queue: Id, how: &Pass, src: Id, dst: Id, cb: &ColorBuffer, n: usize) -> Res<(Stats, f64)> {
        let (mut wall, mut cpu, mut gpu, mut commit) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let p0 = process_cpu_ms();
        for _ in 0..n {
            let c = thread_cpu_ms();
            let (committed, done, g) = copy_once(queue, how, src, dst, cb.width, cb.height)?;
            cpu.push(thread_cpu_ms() - c);
            wall.push(done);
            gpu.push(g);
            commit.push(committed);
        }
        let process_cpu = (process_cpu_ms() - p0) / n as f64;
        Ok((Stats { wall: median(&mut wall), thread_cpu: median(&mut cpu), process_cpu, gpu: median(&mut gpu) }, median(&mut commit)))
    }

    fn compute_pipeline(device: Id) -> Res<Id> {
        let mut err: Id = std::ptr::null_mut();
        let lib = send!(device, "newLibraryWithSource:options:error:", nsstring(KERNEL)? => Id, std::ptr::null_mut() => Id, &raw mut err => *mut Id; Id)?;
        if lib.is_null() {
            return Err(format!("newLibraryWithSource: {}", describe(err)).into());
        }
        let f = send!(lib, "newFunctionWithName:", nsstring("to_bgra")? => Id; Id)?;
        let p = send!(device, "newComputePipelineStateWithFunction:error:", f => Id, &raw mut err => *mut Id; Id)?;
        if p.is_null() { Err(format!("newComputePipelineStateWithFunction: {}", describe(err)).into()) } else { Ok(p) }
    }

    /// The gate for one color buffer: readback, handle, alias, copies, bytes, timings.
    fn gate(r: &gfx::Renderer, p: &Private, id: u32, width: u32, height: u32, n: usize) -> Res<()> {
        println!("\n== {width}x{height} (color buffer {id}) ==");
        let mut cb = create(r, id, width, height)?;
        let mut read = vec![0u8; cb.src.len()];
        let ok = vk_read(p, &cb, &mut read);
        println!("VkRead: {}", if ok && read == cb.src { "matches the upload" } else { "DIFFERS from the upload" });

        // SAFETY: a live VkEmulation and its member, which takes its own lock.
        let handle = unsafe { (p.metal)(p.emulation, id) };
        // SAFETY: as above, a second call (the handle is stable for the buffer's life).
        let again = unsafe { (p.metal)(p.emulation, id) };
        println!("metal handle {handle:?} (second call {})", if again == handle { "same" } else { "DIFFERENT" });
        if handle.is_null() {
            println!("no Metal handle for this color buffer");
            return Ok(());
        }
        println!("handle: {}", inspect(handle));
        println!("handle description: {}", describe(handle).replace('\n', " "));
        if !conforms(handle, "MTLHeap") || send!(handle, "type"; u64)? != HEAP_PLACEMENT {
            println!("not a placement heap: no alias to copy from");
            return Ok(());
        }
        let device = send!(handle, "device"; Id)?;
        println!("device: {}", describe(send!(device, "name"; Id)?));
        let queue = send!(device, "newCommandQueue"; Id)?;
        let rgba = surface(device, width, height, MTL_PIXEL_FORMAT_RGBA8)?;
        let bgra = surface(device, width, height, MTL_PIXEL_FORMAT_BGRA8)?;
        println!("IOSurface stride {} (RGBA), {} (BGRA)", rgba.stride, bgra.stride);
        // Usage changes the layout Metal picks (lossless compression among it): find the one
        // whose bytes are the image's.
        let usages = [
            USAGE_SHADER_READ | USAGE_RENDER_TARGET | USAGE_PIXEL_FORMAT_VIEW,
            USAGE_SHADER_READ | USAGE_SHADER_WRITE | USAGE_RENDER_TARGET | USAGE_PIXEL_FORMAT_VIEW,
            USAGE_SHADER_READ | USAGE_RENDER_TARGET,
            USAGE_SHADER_READ | USAGE_PIXEL_FORMAT_VIEW,
            USAGE_SHADER_READ,
        ];
        let mut chosen = None;
        for usage in usages {
            let src = match alias(handle, &cb, usage) {
                Ok(t) => t,
                Err(e) => {
                    println!("alias usage {usage:#x}: {e}");
                    continue;
                }
            };
            copy_once(queue, &Pass::Blit, src, rgba.texture, width, height)?;
            let same = surface_matches(&rgba, &cb.src, width, height, false)?;
            println!("alias usage {usage:#x}: blit {}", if same { "matches VkRead" } else { "differs" });
            if same && chosen.is_none() {
                chosen = Some(src);
            }
        }
        let Some(src) = chosen else {
            println!("no alias reproduced the image's bytes");
            return Ok(());
        };
        let pipeline = compute_pipeline(device)?;
        copy_once(queue, &Pass::Compute(pipeline), src, bgra.texture, width, height)?;
        let same = surface_matches(&bgra, &cb.src, width, height, true)?;
        println!("compute to BGRA IOSurface: {}", if same { "matches VkRead (red and blue swapped)" } else { "differs" });

        // A screen-like picture next (flat blocks and gradients, which Apple GPUs compress
        // losslessly where noise doesn't): the alias must still read the image's bytes. Timings
        // use it.
        for (i, px) in cb.src.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = ((i % width as usize) as u32, (i / width as usize) as u32);
            px.copy_from_slice(&[(x / 64 * 37) as u8, (y / 64 * 53) as u8, ((x + y) / 8) as u8, 255]);
        }
        r.transfer_write(id, 0, 0, width * 4, 0, gfx::Box3 { x: 0, y: 0, z: 0, w: width, h: height, d: 1 }, 0)?;
        let ok = vk_read(p, &cb, &mut read);
        println!("flat picture: VkRead {}", if ok && read == cb.src { "matches the upload" } else { "DIFFERS from the upload" });
        copy_once(queue, &Pass::Blit, src, rgba.texture, width, height)?;
        println!(
            "flat picture: blit {}",
            if surface_matches(&rgba, &cb.src, width, height, false)? { "matches VkRead" } else { "differs" }
        );
        copy_once(queue, &Pass::Compute(pipeline), src, bgra.texture, width, height)?;
        println!(
            "flat picture: compute {}",
            if surface_matches(&bgra, &cb.src, width, height, true)? { "matches VkRead" } else { "differs" }
        );

        println!("VkRead x{n}: {}", time_vk_read(p, &cb, n)?);
        let (blit, blit_commit) = time_copy(queue, &Pass::Blit, src, rgba.texture, &cb, n)?;
        println!("blit to RGBA IOSurface x{n}: {blit} (encode+commit {blit_commit:.3} ms)");
        let (swz, swz_commit) = time_copy(queue, &Pass::Compute(pipeline), src, bgra.texture, &cb, n)?;
        println!("compute to BGRA IOSurface x{n}: {swz} (encode+commit {swz_commit:.3} ms)");
        // What the encoder does with a read-back frame today (PixelBufferPool::fill for RGBA):
        // one pass into a BGRA buffer, red and blue swapped. A Metal-made BGRA surface skips it.
        let mut bgra_bytes = vec![0u8; read.len()];
        let (mut swap, mut cpu) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for _ in 0..n {
            let (t, c) = (Instant::now(), thread_cpu_ms());
            let row = width as usize * 4;
            sim_core::pixels::swap_red_blue_into(&read, row, &mut bgra_bytes, row, width as usize, height as usize);
            std::hint::black_box(&bgra_bytes);
            swap.push(t.elapsed().as_secs_f64() * 1e3);
            cpu.push(thread_cpu_ms() - c);
        }
        println!("encoder's RGBA-to-BGRA copy x{n}: wall {:.3} ms, this thread {:.3} ms CPU", median(&mut swap), median(&mut cpu));
        shipped_paths(r, p, &cb, n)
    }

    /// The two paths as emu and simee run them, end to end in this process, per frame:
    ///
    /// - readback: `VkRead` into a buffer (emu's ring slot), a copy of it (simee's `pread` from
    ///   the ring), and the encoder's pool fill (`PixelBufferPool::fill`, RGBA to BGRA);
    /// - `-gpu metal`: `Renderer::color_buffer_surface` (alias cache, pool of 3, the compute pass,
    ///   waited for), a Mach port for it sent to an inbox (`MachSender`), received
    ///   (`SurfaceInbox::recv`) and wrapped for VideoToolbox (`PixelBuffer::wrap_surface`).
    fn shipped_paths(r: &gfx::Renderer, p: &Private, cb: &ColorBuffer, n: usize) -> Res<()> {
        use sim_coresim::videotoolbox::{PixelBuffer, PixelBufferPool, PixelFormat};
        let (w, h) = (cb.width, cb.height);
        let first = r.color_buffer_surface(cb.id, w, h).ok_or("no surface from the library path")?;
        let same = first.with_bgra(|px, stride| gfx::metal::bgra_matches_rgba(px, stride, &cb.src, w, h)) == Some(true);
        println!("library surface: {}", if same { "matches the upload (red and blue swapped)" } else { "DIFFERS" });
        drop(first);
        let inbox = sim_coresim::SurfaceInbox::check_in(std::process::id() as i32).map_err(|e| e.to_string())?;
        let sender = gfx::metal::MachSender::look_up(inbox.name()).ok_or("no inbox port")?;
        // (emu side, simee side, wall) per frame.
        let (mut emu, mut simee, mut wall) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        let p0 = process_cpu_ms();
        for seq in 0..n as u32 {
            let (t, c) = (Instant::now(), thread_cpu_ms());
            let s = r.color_buffer_surface(cb.id, w, h).ok_or("no free surface")?;
            if !sender.send(s.mach_port().ok_or("no port")?, seq) {
                return Err("send failed".into());
            }
            let c1 = thread_cpu_ms();
            let (_, got) = inbox.recv(std::time::Duration::from_secs(1)).map_err(|e| e.to_string())?.ok_or("nothing received")?;
            let picture = PixelBuffer::wrap_surface(&got).map_err(|e| e.to_string())?;
            std::hint::black_box(&picture);
            let c2 = thread_cpu_ms();
            wall.push(t.elapsed().as_secs_f64() * 1e3);
            emu.push(c1 - c);
            simee.push(c2 - c1);
            // The client's release: the surface goes back to the pool with its last holder.
            drop((picture, got, s));
        }
        let metal_process = (process_cpu_ms() - p0) / n as f64;
        println!(
            "-gpu metal x{n}: wall {:.3} ms, emu thread {:.3} ms CPU (present + send), simee thread {:.3} ms CPU (receive + wrap), process {metal_process:.3} ms CPU",
            median(&mut wall),
            median(&mut emu),
            median(&mut simee)
        );
        let pool = PixelBufferPool::new(w, h, PixelFormat::Rgba).map_err(|e| e.to_string())?;
        let (mut slot, mut read) = (vec![0u8; cb.src.len()], vec![0u8; cb.src.len()]);
        let (mut emu, mut simee, mut wall) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        let p0 = process_cpu_ms();
        for _ in 0..n {
            let (t, c) = (Instant::now(), thread_cpu_ms());
            if !vk_read(p, cb, &mut slot) {
                return Err("readColorBufferToBytes failed".into());
            }
            let c1 = thread_cpu_ms();
            read.copy_from_slice(&slot);
            let picture = pool.fill(&read, w as usize * 4).map_err(|e| e.to_string())?;
            std::hint::black_box(&picture);
            let c2 = thread_cpu_ms();
            wall.push(t.elapsed().as_secs_f64() * 1e3);
            emu.push(c1 - c);
            simee.push(c2 - c1);
        }
        let read_process = (process_cpu_ms() - p0) / n as f64;
        println!(
            "readback x{n}: wall {:.3} ms, emu thread {:.3} ms CPU (VkRead), simee thread {:.3} ms CPU (ring copy + pool fill), process {read_process:.3} ms CPU",
            median(&mut wall),
            median(&mut emu),
            median(&mut simee)
        );
        println!("saved per frame: {:.3} ms of process CPU", read_process - metal_process);
        Ok(())
    }

    pub fn main() -> Res<()> {
        const USAGE: &str = "usage: metal_handle <lib64> [moltenvk|kosmickrisp] [iterations]";
        let args: Vec<String> = std::env::args().skip(1).collect();
        let lib64 = PathBuf::from(args.first().ok_or(USAGE)?);
        let driver = args.get(1).map_or("moltenvk", String::as_str);
        let n: usize = args.get(2).map_or(Ok(200), |s| s.parse())?;
        let launcher = lib64.parent().ok_or("lib64 has no parent")?;
        let icd = match driver {
            "kosmickrisp" if !gfx::Icd::KosmicKrisp.ships_in(&lib64) => {
                return Err(format!("{} is not installed", lib64.join("vulkan/libkosmickrisp_icd.json").display()).into());
            }
            "kosmickrisp" => gfx::Icd::KosmicKrisp,
            "moltenvk" => gfx::Icd::MoltenVk,
            _ => return Err(USAGE.into()),
        };
        gfx::link_beside_exe(&lib64);
        for n in gfx::configure_environment(launcher, icd) {
            eprintln!("{n}");
        }
        println!("lib64 {} driver {driver} (ICD files {})", lib64.display(), std::env::var("VK_DRIVER_FILES").unwrap_or_default());
        let r = gfx::Renderer::load(&lib64)?;
        // As emu starts it for an API 36 guest: no GL, Vulkan composition.
        let features = "RefCountPipe:enabled,GlDirectMem:enabled,HasSharedSlotsHostMemoryAllocator:enabled,VulkanVirtualQueue:enabled";
        let flags = gfx::flags::USE_EGL | gfx::flags::USE_VK | gfx::flags::VULKAN_NATIVE_SWAPCHAIN;
        let verbose = std::env::var_os("METAL_HANDLE_VERBOSE").is_some();
        let t = Instant::now();
        r.init_with_features(720, 1280, flags, Some(features), verbose, std::sync::Arc::new(|_| {}))?;
        println!("initialized in {:.0} ms (Vulkan {})", t.elapsed().as_secs_f64() * 1e3, if gfx::vulkan_ok() { "up" } else { "failed" });
        let p = resolve(&lib64.join(gfx::BACKEND))?;
        println!("resolved readColorBufferToBytes and getColorBufferMetalMemoryHandle (UUID-checked)");
        gate(&r, &p, 1, 720, 1280, n)?;
        gate(&r, &p, 2, 1080, 2400, n)?;
        gate(&r, &p, 3, 2560, 1600, n)?;
        Ok(())
    }
}
