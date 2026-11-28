//! Guest RAM addressed by guest physical address: an anonymous host mapping, or a shared one of
//! another process's memory fd (a vhost-user frontend's guest RAM, [`GuestRam::map_shared`]).
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::{Error, Result};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("guest memory access out of range: {len} bytes at {addr:#x}")]
pub struct MemError {
    pub addr: u64,
    pub len: u64,
}

/// Most iovecs one `preadv`/`pwritev` takes (macOS and Linux both define `IOV_MAX` as 1024).
#[cfg(unix)]
const IOV_MAX: usize = 1024;

/// A host address range backing guest-visible memory, handed to native libraries (the GPU
/// renderer) that read and write guest buffers directly. Constructed only from live mappings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRange {
    pub(crate) ptr: *mut u8,
    pub(crate) len: usize,
}

// SAFETY: plain address + length; whoever dereferences it upholds the mapping's lifetime.
unsafe impl Send for HostRange {}
// SAFETY: as above.
unsafe impl Sync for HostRange {}

impl HostRange {
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Wraps memory returned by a native library (e.g. a renderer's mapped blob).
    ///
    /// # Safety
    /// `ptr..ptr+len` must stay a valid mapping for as long as the range is used.
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> HostRange {
        HostRange { ptr, len }
    }
}

/// A contiguous block of guest RAM.
///
/// The guest reads and writes this memory while device threads do the same through the
/// accessors here, exactly like DMA on real hardware. Accessors copy bytes in and out and never
/// hand out references into the mapping, so a racing guest can only produce stale or torn
/// *values* — which virtio devices must already tolerate — never undefined behaviour in safe
/// callers. The integer accessors ([`GuestRam::read_u16`] and friends) are never torn at an
/// aligned address: each is one atomic access there.
pub struct GuestRam {
    ptr: *mut u8,
    len: u64,
    base: u64,
    /// The `hv_vm_map` permission flags this RAM is mapped into the VM with, or 0 when it
    /// isn't: [`GuestRam::discard`] has to replace the guest's view of the pages too.
    #[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
    hv_flags: AtomicU64,
    /// A shared mapping of another process's memory ([`GuestRam::map_shared`]): pages are
    /// freed in the file, not replaced in this mapping.
    shared: bool,
}

/// [`GuestRam::discard`] works in host pages; 16 KiB is the host page size on Apple silicon and a
/// multiple of it elsewhere, and the alignment [`GuestRam::new`] requires.
pub const DISCARD_GRANULE: u64 = 0x4000;

// SAFETY: the mapping is plain bytes with no interior references; all access goes through
// copying accessors or, for aligned integers, atomic ones (see the type docs).
unsafe impl Send for GuestRam {}
// SAFETY: as above.
unsafe impl Sync for GuestRam {}

impl GuestRam {
    /// Allocates `len` bytes of zeroed RAM that will appear at guest physical address `base`.
    /// Pages are committed lazily by the host as the guest touches them.
    pub fn new(base: u64, len: u64) -> Result<GuestRam> {
        if len == 0 || !len.is_multiple_of(0x4000) || !base.is_multiple_of(0x4000) {
            return Err(Error::Invalid("guest RAM base and size must be non-zero multiples of 16 KiB"));
        }
        let ptr = sys::alloc(len as usize).ok_or(Error::Mmap(len as usize))?;
        Ok(GuestRam { ptr, len, base, hv_flags: AtomicU64::new(0), shared: false })
    }

    /// Maps `len` bytes of `fd` from `offset` as guest RAM at `base`, shared: the memory another
    /// process (a vhost-user frontend such as QEMU) gives its guest, so device threads here read
    /// and write what the guest sees. The mapping stays valid after `fd` is closed.
    #[cfg(unix)]
    pub fn map_shared(base: u64, len: u64, fd: std::os::fd::BorrowedFd<'_>, offset: u64) -> Result<GuestRam> {
        use std::os::fd::AsRawFd;
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        if len == 0 || !len.is_multiple_of(page) || !offset.is_multiple_of(page) {
            return Err(Error::Invalid("shared guest RAM must be a non-zero whole number of pages at a page offset"));
        }
        let off = libc::off_t::try_from(offset).map_err(|_| Error::Invalid("shared guest RAM offset out of range"))?;
        // SAFETY: a new shared mapping of a caller-provided descriptor, no address hint; the
        // result is checked. The descriptor is borrowed only for the call.
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), off)
        };
        if ptr == libc::MAP_FAILED {
            return Err(Error::Mmap(len as usize));
        }
        Ok(GuestRam { ptr: ptr.cast(), len, base, hv_flags: AtomicU64::new(0), shared: true })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Guest physical address one past the end.
    pub fn end(&self) -> u64 {
        self.base + self.len
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn host_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Records that `Vm::map_ram` mapped this RAM into the guest with `flags`.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn set_hv_mapped(&self, flags: u64) {
        self.hv_flags.store(flags, std::sync::atomic::Ordering::Release);
    }

    /// Gives the host pages behind guest `[addr, addr + len)` back to the host, rounded inward to
    /// whole [`DISCARD_GRANULE`]s, and returns how many bytes that covered. Afterwards the range
    /// reads as zeroes from both the host and the guest, and its pages commit again lazily when
    /// either side touches them.
    ///
    /// The host mapping is replaced with fresh anonymous memory (`mmap(MAP_FIXED)`). For RAM the
    /// VM maps, the guest's view is replaced with it: `hv_vm_unmap`, the new mapping, `hv_vm_map`.
    /// Measured on Apple silicon (`tests/discard_probe.rs`), the alternatives don't work:
    /// `madvise(MADV_DONTNEED | MADV_FREE)` neither lowers `phys_footprint` nor zeroes;
    /// `MADV_FREE_REUSABLE` lowers it only for host-touched pages and keeps the old data (no
    /// zero on read, so the guest could see a freed buffer's contents);
    /// `mmap(MAP_FIXED)` alone leaves the guest on the old pages, no longer shared with the host.
    ///
    /// The guest must not use the range while it is discarded (it has freed it): for the
    /// microseconds the range is unmapped from the VM, a vCPU access exits as an access to an
    /// unmapped address. Host threads may keep copying in and out; they see old data or zeroes.
    pub fn discard(&self, addr: u64, len: u64) -> Result<u64> {
        let off = self.offset(addr, len as usize).map_err(|_| Error::Invalid("discard range outside guest RAM"))? as u64;
        let start = off.next_multiple_of(DISCARD_GRANULE);
        let end = (off + len) / DISCARD_GRANULE * DISCARD_GRANULE;
        if end <= start {
            return Ok(0);
        }
        let n = (end - start) as usize;
        // SAFETY: `start + n` is inside the mapping (checked and rounded inward above).
        let host = unsafe { self.ptr.add(start as usize) };
        if self.shared {
            return remove_shared(host, n).map(|()| n as u64);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let flags = self.hv_flags.load(std::sync::atomic::Ordering::Acquire);
            if flags != 0 {
                let ipa = self.base + start;
                // SAFETY: plain FFI call; unmapping only affects the guest's view of the range.
                let rc = unsafe { crate::ffi::hv_vm_unmap(ipa, n) };
                if rc != 0 {
                    return Err(Error::Hv { call: "hv_vm_unmap", code: rc });
                }
                let replaced = replace_pages(host, n);
                // SAFETY: `host..host + n` is part of this RAM's live mapping (the new pages or,
                // if replacing failed, the old ones), which outlives the VM mapping as
                // `Vm::map_ram` requires.
                let map = || unsafe { crate::ffi::hv_vm_map(host.cast(), ipa, n, flags) };
                // Once more on failure: a range left unmapped is a hole in guest memory.
                let rc = match map() {
                    0 => 0,
                    _ => map(),
                };
                if rc != 0 {
                    return Err(Error::GuestHole { ipa, len: n, code: rc });
                }
                return replaced.map(|()| n as u64);
            }
        }
        replace_pages(host, n).map(|()| n as u64)
    }

    /// The host address range behind guest `[addr, addr + len)`, for native code that DMAs
    /// into guest buffers. Valid while this `GuestRam` lives.
    pub fn host_range(&self, addr: u64, len: usize) -> Option<HostRange> {
        let off = self.offset(addr, len).ok()?;
        // SAFETY: `off + len` is inside the mapping (checked above).
        Some(HostRange { ptr: unsafe { self.ptr.add(off) }, len })
    }

    fn offset(&self, addr: u64, len: usize) -> std::result::Result<usize, MemError> {
        let err = MemError { addr, len: len as u64 };
        let off = addr.checked_sub(self.base).ok_or(err.clone_err())?;
        let end = off.checked_add(len as u64).ok_or(err.clone_err())?;
        if end > self.len {
            return Err(err);
        }
        Ok(off as usize)
    }

    /// Copies guest memory at `addr` into `buf`.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, buf.len())?;
        // SAFETY: `off + buf.len()` is within the mapping (checked above) and `buf` is a
        // distinct host allocation, so the ranges cannot overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(off), buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    /// Appends guest `[addr, addr + len)` to `out` without zero-filling it first (device paths
    /// copy whole packets and commands out of the guest this way).
    pub fn read_append(&self, addr: u64, len: usize, out: &mut Vec<u8>) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, len)?;
        out.reserve(len);
        let start = out.len();
        // SAFETY: `off + len` is within the mapping (checked above); `reserve` made room for `len`
        // bytes after `start` in `out`'s own allocation, which cannot overlap guest memory, and
        // every one of them is initialized by the copy before `set_len` exposes them.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(off), out.as_mut_ptr().add(start), len);
            out.set_len(start + len);
        }
        Ok(())
    }

    /// Copies `buf` into guest memory at `addr`.
    pub fn write(&self, addr: u64, buf: &[u8]) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, buf.len())?;
        // SAFETY: as in `read`, with the direction reversed.
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), self.ptr.add(off), buf.len()) };
        Ok(())
    }

    /// Reads `len` bytes from `file` at `offset` straight into guest memory at `addr` (no
    /// intermediate buffer). Short reads at end of file zero-fill the rest.
    pub fn read_from_file(&self, addr: u64, len: usize, file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        let off = self.offset(addr, len).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut done = 0usize;
        while done < len {
            // SAFETY: the destination range lies inside the mapping (checked above); the read
            // writes at most `len - done` bytes there.
            let n = match unsafe { sys::pread(file, self.ptr.add(off + done), len - done, offset + done as u64) } {
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if n == 0 {
                // Past end of file: the rest reads as zeroes, like a sparse disk.
                // SAFETY: same range as above.
                unsafe { std::ptr::write_bytes(self.ptr.add(off + done), 0, len - done) };
                break;
            }
            done += n;
        }
        Ok(())
    }

    /// Writes `len` bytes of guest memory at `addr` to `file` at `offset` (no intermediate buffer).
    pub fn write_to_file(&self, addr: u64, len: usize, file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        let off = self.offset(addr, len).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut done = 0usize;
        while done < len {
            // SAFETY: the source range lies inside the mapping (checked above).
            let n = match unsafe { sys::pwrite(file, self.ptr.add(off + done), len - done, offset + done as u64) } {
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "pwrite wrote nothing"));
            }
            done += n;
        }
        Ok(())
    }

    /// Reads `file` from `offset` into the guest ranges `segs` (`(addr, len)`, in order) with
    /// as few `preadv` calls as possible: guest-adjacent ranges merge into one iovec and at
    /// most `IOV_MAX` (1024) go in one call. Short reads continue where they stopped; past end of
    /// file the rest reads as zeroes, as in [`GuestRam::read_from_file`]. Every range is
    /// checked before anything is read.
    pub fn read_vectored_from_file(&self, segs: &[(u64, usize)], file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        self.vectored(segs, file, offset, false)
    }

    /// Writes the guest ranges `segs` (`(addr, len)`, in order) to `file` from `offset` with
    /// as few `pwritev` calls as possible (see [`GuestRam::read_vectored_from_file`]).
    pub fn write_vectored_to_file(&self, segs: &[(u64, usize)], file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        self.vectored(segs, file, offset, true)
    }

    /// Windows has no `preadv`: every range is checked first, then each goes through the
    /// positional read or write one after the other.
    #[cfg(windows)]
    fn vectored(&self, segs: &[(u64, usize)], file: &std::fs::File, offset: u64, write: bool) -> std::io::Result<()> {
        let invalid = |e: MemError| std::io::Error::new(std::io::ErrorKind::InvalidInput, e);
        for &(addr, len) in segs {
            self.offset(addr, len).map_err(invalid)?;
        }
        let mut pos = offset;
        for &(addr, len) in segs {
            if write {
                self.write_to_file(addr, len, file, pos)?;
            } else {
                self.read_from_file(addr, len, file, pos)?;
            }
            pos += len as u64;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn vectored(&self, segs: &[(u64, usize)], file: &std::fs::File, offset: u64, write: bool) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let invalid = |e: MemError| std::io::Error::new(std::io::ErrorKind::InvalidInput, e);
        let mut iov: Vec<libc::iovec> = Vec::with_capacity(segs.len().min(IOV_MAX));
        for &(addr, len) in segs {
            let off = self.offset(addr, len).map_err(invalid)?;
            if len == 0 {
                continue;
            }
            // SAFETY: `off + len` is inside the mapping (checked above).
            let base = unsafe { self.ptr.add(off) };
            match iov.last_mut() {
                // SAFETY: `last.iov_base + iov_len` is the end of a range checked to lie inside the mapping.
                Some(last) if unsafe { last.iov_base.cast::<u8>().add(last.iov_len) } == base => last.iov_len += len,
                _ => iov.push(libc::iovec { iov_base: base.cast(), iov_len: len }),
            }
        }
        let fd = file.as_raw_fd();
        let (mut first, mut pos) = (0usize, offset);
        while first < iov.len() {
            let batch = &iov[first..iov.len().min(first + IOV_MAX)];
            // SAFETY: every iovec covers a range inside the mapping (checked above); preadv
            // writes and pwritev reads only within them, and `batch.len() <= IOV_MAX` fits a c_int.
            let n = unsafe {
                if write {
                    libc::pwritev(fd, batch.as_ptr(), batch.len() as libc::c_int, pos as libc::off_t)
                } else {
                    libc::preadv(fd, batch.as_ptr(), batch.len() as libc::c_int, pos as libc::off_t)
                }
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                if write {
                    return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "pwritev wrote nothing"));
                }
                // Past end of file: the rest reads as zeroes, like a sparse disk.
                for v in &iov[first..] {
                    // SAFETY: each iovec lies inside the mapping (checked above).
                    unsafe { std::ptr::write_bytes(v.iov_base.cast::<u8>(), 0, v.iov_len) };
                }
                return Ok(());
            }
            // Skip what was transferred; a partly done iovec continues where it stopped.
            let mut n = n as usize;
            pos += n as u64;
            while n > 0 && first < iov.len() {
                let v = &mut iov[first];
                if n >= v.iov_len {
                    n -= v.iov_len;
                    first += 1;
                } else {
                    // SAFETY: `n < iov_len`, so the advanced base stays inside the same range.
                    v.iov_base = unsafe { v.iov_base.cast::<u8>().add(n) }.cast();
                    v.iov_len -= n;
                    n = 0;
                }
            }
        }
        Ok(())
    }

    /// Fills `len` bytes at `addr` with zeroes.
    pub fn zero(&self, addr: u64, len: usize) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, len)?;
        // SAFETY: range checked above.
        unsafe { std::ptr::write_bytes(self.ptr.add(off), 0, len) };
        Ok(())
    }

    /// The host address of the atomic `A` at guest `addr`, or `None` when the host address isn't
    /// aligned for `A` (the caller copies bytes instead). Checked against the atomic's alignment,
    /// which can exceed the integer's (`AtomicU64` on 32-bit x86).
    fn aligned<A>(&self, addr: u64) -> std::result::Result<Option<*mut A>, MemError> {
        let off = self.offset(addr, size_of::<A>())?;
        // SAFETY: `off + size_of::<A>()` is within the mapping (checked above).
        let p = unsafe { self.ptr.add(off) }.cast::<A>();
        Ok(p.is_aligned().then_some(p))
    }

    // The integer accessors are single accesses where the address is aligned: virtqueue
    // indices, flags and event words are published by one side while the other polls them, and
    // a byte-wise copy (what `read`/`write` do for a length known only at run time) lets a
    // reader see half of each value: `used.idx` going 0x01ff -> 0x0200 read as 0x0100. Relaxed
    // atomics are single-copy atomic, and they're what the queues' fences order. Unaligned
    // addresses keep the byte copy.

    pub fn read_u16(&self, addr: u64) -> std::result::Result<u16, MemError> {
        if let Some(p) = self.aligned::<AtomicU16>(addr)? {
            // SAFETY: `p` is aligned and in the mapping, which lives as long as `self`; other
            // accesses to it are the guest's or these accessors' (see the type docs).
            return Ok(u16::from_le(unsafe { &*p }.load(Ordering::Relaxed)));
        }
        let mut b = [0; 2];
        self.read(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn read_u32(&self, addr: u64) -> std::result::Result<u32, MemError> {
        if let Some(p) = self.aligned::<AtomicU32>(addr)? {
            // SAFETY: as in `read_u16`.
            return Ok(u32::from_le(unsafe { &*p }.load(Ordering::Relaxed)));
        }
        let mut b = [0; 4];
        self.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn read_u64(&self, addr: u64) -> std::result::Result<u64, MemError> {
        if let Some(p) = self.aligned::<AtomicU64>(addr)? {
            // SAFETY: as in `read_u16`.
            return Ok(u64::from_le(unsafe { &*p }.load(Ordering::Relaxed)));
        }
        let mut b = [0; 8];
        self.read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn write_u16(&self, addr: u64, v: u16) -> std::result::Result<(), MemError> {
        if let Some(p) = self.aligned::<AtomicU16>(addr)? {
            // SAFETY: as in `read_u16`.
            unsafe { &*p }.store(v.to_le(), Ordering::Relaxed);
            return Ok(());
        }
        self.write(addr, &v.to_le_bytes())
    }

    pub fn write_u32(&self, addr: u64, v: u32) -> std::result::Result<(), MemError> {
        if let Some(p) = self.aligned::<AtomicU32>(addr)? {
            // SAFETY: as in `read_u16`.
            unsafe { &*p }.store(v.to_le(), Ordering::Relaxed);
            return Ok(());
        }
        self.write(addr, &v.to_le_bytes())
    }
}

impl GuestRam {
    /// How many bytes of the host pages behind guest `[addr, addr + len)` are resident
    /// (`mincore`), counting whole host pages; `None` when the range is outside the RAM or the
    /// call fails. Tells whether freed memory really went back to the host. Always `None` on
    /// Windows, which has no equivalent this crate reads yet.
    #[cfg(windows)]
    pub fn resident(&self, addr: u64, len: u64) -> Option<u64> {
        let _ = self.offset(addr, len as usize).ok()?;
        None
    }

    /// How many bytes of the host pages behind guest `[addr, addr + len)` are resident
    /// (`mincore`), counting whole host pages; `None` when the range is outside the RAM or the
    /// call fails. Tells whether freed memory really went back to the host.
    #[cfg(unix)]
    pub fn resident(&self, addr: u64, len: u64) -> Option<u64> {
        let off = self.offset(addr, len as usize).ok()?;
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = usize::try_from(page).ok().filter(|&p| p > 0)?;
        let start = off / page * page;
        let n = (off + len as usize).next_multiple_of(page).min(self.len as usize) - start;
        let mut vec = vec![0u8; n.div_ceil(page)];
        // SAFETY: `start..start + n` lies inside the mapping (page-aligned start, end clamped to
        // the mapping, whose length is a page multiple) and `vec` holds one byte per page.
        let rc = unsafe { libc::mincore(self.ptr.add(start).cast(), n, vec.as_mut_ptr().cast()) };
        (rc == 0).then(|| vec.iter().filter(|&&v| v & 1 != 0).count() as u64 * page as u64)
    }
}

/// Replaces `n` bytes of anonymous memory at `host` with fresh zero-fill-on-demand pages, which
/// releases the old ones. On failure the old pages stay mapped.
#[cfg(windows)]
fn replace_pages(host: *mut u8, n: usize) -> Result<()> {
    // SAFETY: `host..host + n` is page aligned and inside the allocation `GuestRam::new` made.
    if unsafe { crate::win::recommit(host, n) } { Ok(()) } else { Err(Error::Mmap(n)) }
}

#[cfg(unix)]
fn replace_pages(host: *mut u8, n: usize) -> Result<()> {
    // SAFETY: `host..host + n` is page aligned and lies inside a private anonymous mapping this
    // module owns; MAP_FIXED replaces exactly that range in place, so no other mapping is touched
    // and the accessors keep pointing at mapped memory.
    let p = unsafe {
        libc::mmap(host.cast(), n, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED, -1, 0)
    };
    if p == libc::MAP_FAILED {
        return Err(Error::Mmap(n));
    }
    Ok(())
}

/// Frees the pages behind `n` bytes of a shared file mapping at `host`, in the file itself, so
/// every process mapping it reads zeroes there afterwards (`MADV_REMOVE`, Linux's hole punch
/// through a mapping; elsewhere there's no equivalent and nothing is freed).
fn remove_shared(host: *mut u8, n: usize) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `host..host + n` is page aligned and inside a shared mapping this module owns.
        if unsafe { libc::madvise(host.cast(), n, libc::MADV_REMOVE) } != 0 {
            return Err(Error::Invalid("MADV_REMOVE failed on shared guest RAM"));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (host, n);
        Err(Error::Invalid("discarding shared guest RAM needs Linux's MADV_REMOVE"))
    }
}

/// The calling process's `phys_footprint` in bytes (Activity Monitor's Memory column, what jetsam
/// limits), for measuring what [`GuestRam::discard`] gives back. Measured caveat
/// (`tests/discard_probe.rs`): a page of VM-mapped RAM that both the host and the guest touched
/// counts twice.
#[cfg(target_os = "macos")]
pub fn phys_footprint() -> Option<u64> {
    // SAFETY: rusage_info_v2 is plain integers, for which all-zeroes is valid.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is a live rusage_info_v2, the size RUSAGE_INFO_V2 writes.
    let rc = unsafe { libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, (&mut info as *mut libc::rusage_info_v2).cast()) };
    (rc == 0).then_some(info.ri_phys_footprint)
}

/// Not read on Windows yet.
#[cfg(windows)]
pub fn phys_footprint() -> Option<u64> {
    None
}

/// Resident set size of the calling process in bytes, from `/proc/self/statm`.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn phys_footprint() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * page.max(0) as u64)
}

impl MemError {
    fn clone_err(&self) -> MemError {
        MemError { addr: self.addr, len: self.len }
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what `sys::alloc` or `GuestRam::map_shared` returned.
        unsafe { sys::free(self.ptr, self.len as usize) };
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    pub(super) fn alloc(len: usize) -> Option<*mut u8> {
        // SAFETY: anonymous private mapping with no address hint; the result is checked.
        let ptr =
            unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        (ptr != libc::MAP_FAILED).then_some(ptr.cast())
    }

    /// # Safety
    /// `ptr`/`len` are exactly what a mapping call returned, and nothing uses it afterwards.
    pub(super) unsafe fn free(ptr: *mut u8, len: usize) {
        // SAFETY: the caller's contract.
        unsafe { libc::munmap(ptr.cast(), len) };
    }

    /// # Safety
    /// `ptr..ptr + len` is writable memory.
    pub(super) unsafe fn pread(file: &File, ptr: *mut u8, len: usize, offset: u64) -> io::Result<usize> {
        // SAFETY: the caller's contract; the descriptor is owned by `file`, which outlives the call.
        let n = unsafe { libc::pread(file.as_raw_fd(), ptr.cast(), len, offset as libc::off_t) };
        usize::try_from(n).map_err(|_| io::Error::last_os_error())
    }

    /// # Safety
    /// `ptr..ptr + len` is readable memory.
    pub(super) unsafe fn pwrite(file: &File, ptr: *const u8, len: usize, offset: u64) -> io::Result<usize> {
        // SAFETY: the caller's contract; the descriptor is owned by `file`, which outlives the call.
        let n = unsafe { libc::pwrite(file.as_raw_fd(), ptr.cast(), len, offset as libc::off_t) };
        usize::try_from(n).map_err(|_| io::Error::last_os_error())
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::FileExt;

    pub(super) fn alloc(len: usize) -> Option<*mut u8> {
        crate::win::alloc_zeroed(len)
    }

    /// # Safety
    /// `ptr` is exactly what `alloc` returned, and nothing uses it afterwards.
    pub(super) unsafe fn free(ptr: *mut u8, _len: usize) {
        // SAFETY: the caller's contract.
        unsafe { crate::win::release(ptr) };
    }

    /// # Safety
    /// `ptr..ptr + len` is writable memory.
    pub(super) unsafe fn pread(file: &File, ptr: *mut u8, len: usize, offset: u64) -> io::Result<usize> {
        // SAFETY: the caller's contract.
        file.seek_read(unsafe { std::slice::from_raw_parts_mut(ptr, len) }, offset)
    }

    /// # Safety
    /// `ptr..ptr + len` is readable memory.
    pub(super) unsafe fn pwrite(file: &File, ptr: *const u8, len: usize, offset: u64) -> io::Result<usize> {
        // SAFETY: the caller's contract.
        file.seek_write(unsafe { std::slice::from_raw_parts(ptr, len) }, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_append_extends_without_touching_what_is_there() {
        let ram = GuestRam::new(0x4000_0000, 0x8000).expect("alloc");
        ram.write(0x4000_0100, b"guest").expect("write");
        let mut out = b"host:".to_vec();
        ram.read_append(0x4000_0100, 5, &mut out).expect("append");
        assert_eq!(out, b"host:guest");
        assert!(ram.read_append(0x4000_7ffe, 4, &mut out).is_err(), "past the end");
        assert_eq!(out, b"host:guest", "unchanged on error");
        ram.read_append(0x4000_0000, 0, &mut out).expect("empty");
    }

    fn tmp_file(name: &str, data: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!("emu-hvf-ram-{}-{name}", std::process::id()));
        std::fs::write(&path, data).expect("write");
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).expect("open");
        (path, file)
    }

    #[test]
    fn vectored_reads_scatter_merge_and_zero_fill_past_eof() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x10_0000).expect("alloc");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let (path, file) = tmp_file("readv", &data);
        ram.write(B, &vec![0xee; 0x10_0000]).expect("fill");
        // Adjacent pair (merged), a gap, an empty range, then a range running past EOF.
        let segs = [(B + 0x100, 1000), (B + 0x100 + 1000, 24), (B + 0x8000, 0), (B + 0x9000, 3000), (B + 0x20000, 8000)];
        ram.read_vectored_from_file(&segs, &file, 100).expect("readv");
        let mut off = 100usize;
        for &(addr, len) in &segs {
            let mut got = vec![0u8; len];
            ram.read(addr, &mut got).expect("r");
            let want: Vec<u8> = (off..off + len).map(|i| data.get(i).copied().unwrap_or(0)).collect();
            assert_eq!(got, want, "segment at {addr:#x}");
            off += len;
        }
        let mut gap = [0u8; 4];
        ram.read(B + 0x100 + 1024, &mut gap).expect("r");
        assert_eq!(gap, [0xee; 4], "bytes between segments untouched");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn vectored_io_spans_more_than_iov_max_ranges() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x40_0000).expect("alloc");
        // 2500 non-adjacent 16-byte ranges: three preadv/pwritev batches.
        let segs: Vec<(u64, usize)> = (0..2500u64).map(|i| (B + i * 32, 16)).collect();
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
        let (path, file) = tmp_file("iovmax", &data);
        ram.read_vectored_from_file(&segs, &file, 0).expect("readv");
        for (i, &(addr, _)) in segs.iter().enumerate() {
            let mut got = [0u8; 16];
            ram.read(addr, &mut got).expect("r");
            assert_eq!(&got[..], &data[i * 16..i * 16 + 16], "segment {i}");
        }
        // Write them back shifted by one byte of file offset.
        ram.write_vectored_to_file(&segs, &file, 1).expect("writev");
        let back = std::fs::read(&path).expect("read");
        assert_eq!(&back[1..40_001], &data[..]);
        assert_eq!(back[0], data[0]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn vectored_io_checks_every_range_first() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x8000).expect("alloc");
        let (path, file) = tmp_file("bounds", &[9u8; 64]);
        let segs = [(B, 16), (B + 0x7ff8, 16)];
        assert!(ram.read_vectored_from_file(&segs, &file, 0).is_err());
        assert_eq!(ram.read_u32(B), Ok(0), "nothing read when a later range is bad");
        ram.write(B, &[1u8; 16]).expect("w");
        assert!(ram.write_vectored_to_file(&segs, &file, 0).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), [9u8; 64], "nothing written");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn discard_zeroes_whole_granules_inside_the_range_only() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x10_0000).expect("alloc");
        ram.write(B, &vec![0x5a; 0x10_0000]).expect("fill");
        // 0x2000..0x1_2000 rounds inward to 0x4000..0x1_0000.
        assert_eq!(ram.discard(B + 0x2000, 0x1_0000).expect("discard"), 0xc000);
        let mut back = vec![0u8; 0x10_0000];
        ram.read(B, &mut back).expect("read");
        assert!(back[..0x4000].iter().all(|&b| b == 0x5a), "partial head granule kept");
        assert!(back[0x4000..0x1_0000].iter().all(|&b| b == 0), "whole granules read zero");
        assert!(back[0x1_0000..].iter().all(|&b| b == 0x5a), "tail kept");
        // The discarded range is ordinary memory again.
        ram.write(B + 0x8000, b"again").expect("write");
        assert_eq!(ram.read_u32(B + 0x8000), Ok(u32::from_le_bytes(*b"agai")));
        assert_eq!(ram.discard(B + 0x100, 0x3000).expect("small"), 0, "no whole granule");
        assert!(ram.discard(B + 0xf_c000, 0x8000).is_err(), "past the end");
    }

    // Residency is read with `mincore` and `/proc`, which Windows lacks here.
    #[cfg(unix)]
    #[test]
    fn discard_returns_pages_to_the_host() {
        const B: u64 = 0x4000_0000;
        const LEN: u64 = 64 << 20;
        let ram = GuestRam::new(B, LEN).expect("alloc");
        let before = phys_footprint().expect("footprint");
        ram.write(B, &vec![1u8; LEN as usize]).expect("touch");
        let touched = phys_footprint().expect("footprint");
        assert_eq!(ram.discard(B, LEN).expect("discard"), LEN);
        let after = phys_footprint().expect("footprint");
        assert!(touched >= before + LEN * 3 / 4, "touching commits: {before} -> {touched}");
        assert!(after + LEN * 3 / 4 <= touched, "discard releases: {touched} -> {after}");
        assert_eq!(ram.resident(B, LEN), Some(0), "no page of the range is resident");
        assert_eq!(ram.read_u64(B + LEN / 2), Ok(0));
        assert!(ram.resident(B, LEN).is_some_and(|r| r > 0 && r <= 0x4000), "one page back after a read");
    }

    #[cfg(unix)]
    #[test]
    fn resident_counts_touched_pages() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x10_0000).expect("alloc");
        assert_eq!(ram.resident(B, 0x10_0000), Some(0), "untouched");
        ram.write(B + 0x8000, &[1u8; 0x8000]).expect("touch");
        let r = ram.resident(B, 0x10_0000).expect("resident");
        assert!((0x8000..=0x10000).contains(&r), "{r:#x}");
        assert_eq!(ram.resident(B, 0x20_0000), None, "outside the RAM");
    }

    /// Two shared mappings of one memfd, as QEMU's guest RAM and a vhost-user backend's view of
    /// it: writes show through both ways, and a discard frees the pages for both.
    #[cfg(target_os = "linux")]
    #[test]
    fn shared_ram_is_one_memory_for_both_sides() {
        use std::os::fd::AsFd;
        const B: u64 = 0;
        let fd = rustix::fs::memfd_create("guest-ram", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
        rustix::fs::ftruncate(&fd, 0x40_0000).expect("size");
        let frontend = GuestRam::map_shared(B, 0x40_0000, fd.as_fd(), 0).expect("frontend map");
        let backend = GuestRam::map_shared(0x10_0000, 0x20_0000, fd.as_fd(), 0x10_0000).expect("backend map at an offset");
        drop(fd);
        frontend.write(0x10_0010, b"from the guest").expect("w");
        let mut got = [0u8; 14];
        backend.read(0x10_0010, &mut got).expect("r");
        assert_eq!(&got, b"from the guest", "same guest address, same bytes, fd already closed");
        backend.write(0x20_0000, &[7u8; 0x10_0000]).expect("w");
        assert_eq!(frontend.read_u32(0x20_0000), Ok(0x0707_0707));
        assert_eq!(backend.discard(0x20_0000, 0x10_0000).expect("discard"), 0x10_0000);
        assert_eq!(frontend.resident(0x20_0000, 0x10_0000), Some(0), "freed in the file, so for the other side too");
        assert_eq!(frontend.read_u64(0x28_0000), Ok(0));
        let fd = rustix::fs::memfd_create("x", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
        assert!(GuestRam::map_shared(0, 0x1001, fd.as_fd(), 0).is_err(), "not whole pages");
        assert!(GuestRam::map_shared(0, 0x1000, fd.as_fd(), 0x10).is_err(), "unaligned offset");
    }

    #[test]
    fn round_trips_and_bounds_checks() {
        let ram = GuestRam::new(0x4000_0000, 0x8000).expect("alloc");
        ram.write(0x4000_0010, &[1, 2, 3, 4]).expect("write");
        assert_eq!(ram.read_u32(0x4000_0010), Ok(0x0403_0201));
        assert!(ram.read_u32(0x3fff_fffe).is_err(), "below base");
        assert!(ram.read_u32(0x4000_7ffe).is_err(), "straddles the end");
        assert!(ram.write(u64::MAX, &[0]).is_err(), "overflow");
    }

    #[test]
    fn unaligned_integers_round_trip_little_endian() {
        let ram = GuestRam::new(0x4000_0000, 0x8000).expect("alloc");
        ram.write_u16(0x4000_0101, 0xbeef).expect("u16");
        ram.write_u32(0x4000_0203, 0xdead_beef).expect("u32");
        ram.write(0x4000_0305, &0x0102_0304_0506_0708u64.to_le_bytes()).expect("u64");
        let mut b = [0u8; 2];
        ram.read(0x4000_0101, &mut b).expect("bytes");
        assert_eq!(b, [0xef, 0xbe], "little-endian in guest memory");
        assert_eq!(ram.read_u16(0x4000_0101), Ok(0xbeef));
        assert_eq!(ram.read_u32(0x4000_0203), Ok(0xdead_beef));
        assert_eq!(ram.read_u64(0x4000_0305), Ok(0x0102_0304_0506_0708));
        ram.write_u32(0x4000_0400, 0x0403_0201).expect("aligned u32");
        assert_eq!(ram.read_u16(0x4000_0402), Ok(0x0403), "aligned writes are little-endian too");
        assert!(ram.write_u16(0x4000_7fff, 0).is_err(), "straddles the end");
        assert!(ram.read_u64(0x4000_7ffc).is_err(), "straddles the end");
    }

    /// A writer thread flips an aligned integer between two values that differ in every byte
    /// lane a carry crosses (as a virtqueue index does going 0x01ff -> 0x0200) while this thread
    /// reads it: every read is one of the two, never half of each.
    fn never_torn<T: Copy + PartialEq + std::fmt::Debug + Send + 'static>(
        a: T,
        b: T,
        write: fn(&GuestRam, u64, T),
        read: fn(&GuestRam, u64) -> T,
    ) {
        const AT: u64 = 0x4000_0008;
        let ram = std::sync::Arc::new(GuestRam::new(0x4000_0000, 0x8000).expect("alloc"));
        write(&ram, AT, a);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (ram, stop) = (ram.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    write(&ram, AT, b);
                    write(&ram, AT, a);
                }
            })
        };
        let start = std::time::Instant::now();
        let mut torn = None;
        for _ in 0..2_000_000 {
            let v = read(&ram, AT);
            if v != a && v != b {
                torn = Some(v);
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(2) {
                break;
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().expect("writer");
        assert_eq!(torn, None, "read half of {a:x?} and half of {b:x?}");
    }

    #[test]
    fn aligned_u16_reads_and_writes_are_never_torn() {
        never_torn(0x01ffu16, 0x0200, |r, at, v| r.write_u16(at, v).expect("w"), |r, at| r.read_u16(at).expect("r"));
    }

    #[test]
    fn aligned_u32_reads_and_writes_are_never_torn() {
        never_torn(0x00ff_ffffu32, 0x0100_0000, |r, at, v| r.write_u32(at, v).expect("w"), |r, at| r.read_u32(at).expect("r"));
    }

    #[test]
    fn aligned_u64_reads_are_never_torn() {
        // No `write_u64`: the writer stores whole words as an atomic would (a guest vCPU's `str`).
        never_torn(
            0x0000_0000_ffff_ffffu64,
            0x0000_0001_0000_0000,
            |r, at, v| {
                let p = r.host_range(at, 8).expect("range").as_ptr().cast::<u64>();
                // SAFETY: `p` is 8-byte aligned (`AT`, a page-aligned mapping) and in the mapping.
                unsafe { std::sync::atomic::AtomicU64::from_ptr(p) }.store(v.to_le(), std::sync::atomic::Ordering::Relaxed);
            },
            |r, at| r.read_u64(at).expect("r"),
        );
    }
}
