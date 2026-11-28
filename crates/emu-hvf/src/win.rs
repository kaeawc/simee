//! The Win32 calls guest RAM, the shared map and process checks need: a few kernel32 functions,
//! declared here instead of pulling in a binding crate.
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;

type Handle = *mut c_void;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;
const FILE_MAP_WRITE: u32 = 0x0002;
const FILE_MAP_READ: u32 = 0x0004;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const STILL_ACTIVE: u32 = 259;
const ERROR_ACCESS_DENIED: i32 = 5;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32;
    fn CreateFileMappingW(file: Handle, attributes: *mut c_void, protect: u32, size_high: u32, size_low: u32, name: *const u16) -> Handle;
    fn MapViewOfFile(mapping: Handle, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
}

/// `len` bytes of zeroed, committed, private read-write memory at an address the system picks
/// (`None` when the commit limit refuses). The system zero-fills pages as they are first
/// touched, as anonymous `mmap` does.
pub(crate) fn alloc_zeroed(len: usize) -> Option<*mut u8> {
    // SAFETY: a new reservation with no address hint; the result is checked.
    let p = unsafe { VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    (!p.is_null()).then_some(p.cast())
}

/// Releases a whole allocation returned by [`alloc_zeroed`].
///
/// # Safety
/// `ptr` is exactly what `alloc_zeroed` returned and nothing uses the memory afterwards.
pub(crate) unsafe fn release(ptr: *mut u8) {
    // SAFETY: the caller's contract; MEM_RELEASE with size 0 frees the whole reservation.
    unsafe { VirtualFree(ptr.cast(), 0, MEM_RELEASE) };
}

/// Gives the physical pages behind `len` bytes at `ptr` back to the system and commits the range
/// again, which reads as zeroes. Fails only when the system refuses to commit the range anew,
/// and then it stays decommitted.
///
/// # Safety
/// `ptr..ptr + len` is page aligned and inside one allocation from [`alloc_zeroed`].
pub(crate) unsafe fn recommit(ptr: *mut u8, len: usize) -> bool {
    // SAFETY: the caller's contract; decommitting and recommitting touch only that range.
    unsafe { VirtualFree(ptr.cast(), len, MEM_DECOMMIT) != 0 && !VirtualAlloc(ptr.cast(), len, MEM_COMMIT, PAGE_READWRITE).is_null() }
}

/// Maps the first `len` bytes of `file` shared and read-write. The view keeps the mapping
/// object alive, so the handle to it is closed straight away.
pub(crate) fn map_file(file: &File, len: usize) -> io::Result<*mut u8> {
    let len64 = len as u64;
    // SAFETY: the file handle is valid for the call (borrowed from `file`); no attributes, no name.
    let mapping = unsafe {
        CreateFileMappingW(file.as_raw_handle(), std::ptr::null_mut(), PAGE_READWRITE, (len64 >> 32) as u32, len64 as u32, std::ptr::null())
    };
    if mapping.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `mapping` is the live handle just created; the result is checked.
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, len) };
    let err = io::Error::last_os_error();
    // SAFETY: closing the handle created above, once; a mapped view holds its own reference.
    unsafe { CloseHandle(mapping) };
    if view.is_null() { Err(err) } else { Ok(view.cast()) }
}

/// Unmaps a view returned by [`map_file`].
///
/// # Safety
/// `ptr` is exactly what `map_file` returned and nothing uses the view afterwards.
pub(crate) unsafe fn unmap_view(ptr: *mut u8) {
    // SAFETY: the caller's contract.
    unsafe { UnmapViewOfFile(ptr.cast()) };
}

/// Whether process `pid` exists and hasn't exited. A process this one may not query (another
/// user's, a protected one) exists.
pub(crate) fn process_alive(pid: u32) -> bool {
    // SAFETY: plain value arguments; the result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED);
    }
    let mut code = 0u32;
    // SAFETY: `process` is the live handle just opened and `code` a live local.
    let queried = unsafe { GetExitCodeProcess(process, &mut code) } != 0;
    // SAFETY: closing the handle opened above, once.
    unsafe { CloseHandle(process) };
    // A handle can outlive its process, so an exit code other than STILL_ACTIVE says it is gone.
    !queried || code == STILL_ACTIVE
}
