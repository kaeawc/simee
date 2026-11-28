//! IOSurfaces between processes over Mach: how an emulator hands a frame to simee without copying
//! it (`sim_core::stream::Record::Surface`, `emu -gpu metal`, issue #356).
//!
//! The receiving side checks a fresh bootstrap name in ([`SurfaceInbox::check_in`]): launchd
//! makes the name for it and gives it the port's receive right, as Chromium's Mach port rendezvous
//! does. It tells the sender the name over the display socket (`surfaces <name>`). The sender
//! looks the name up ([`SurfaceOutbox::look_up`]) and, per frame, moves a send right for the
//! surface (`IOSurfaceCreateMachPort`) to the port in a message with id
//! [`sim_core::stream::SURFACE_MSG_ID`]: one port descriptor, then the frame's sequence number.
//! The receiver turns the right back into the surface (`IOSurfaceLookupFromMachPort`).
//! `emu_gfxstream::metal` holds emu's own sender, with the same message layout.

use std::ffi::{CString, c_char, c_void};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use sim_core::stream::SURFACE_MSG_ID;

use crate::display::Surface;
use crate::error::{Error, Result};

type MachPort = u32;
type KernReturn = i32;

const MACH_MSG_TYPE_MOVE_SEND: u32 = 17;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
/// A received send right (`MACH_MSG_TYPE_PORT_SEND`).
const MACH_MSG_TYPE_PORT_SEND: u8 = 17;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_SEND_MSG: i32 = 1;
const MACH_RCV_MSG: i32 = 2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_RCV_TIMEOUT: i32 = 0x100;
const MACH_RCV_TIMED_OUT: KernReturn = 0x1000_4003;
const MACH_RCV_TOO_LARGE: KernReturn = 0x1000_4004;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Header {
    bits: u32,
    size: u32,
    remote: MachPort,
    local: MachPort,
    voucher: MachPort,
    id: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PortDescriptor {
    name: MachPort,
    pad1: u32,
    pad2: u16,
    disposition: u8,
    kind: u8,
}

/// The surface message: a complex message with one port and the sequence number.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Message {
    header: Header,
    descriptors: u32,
    port: PortDescriptor,
    seq: u32,
}

/// A received [`Message`] and the kernel's audit trailer right after it (at the message's size,
/// rounded to 4 bytes: `Message` is 44, so no gap).
#[repr(C)]
#[derive(Default)]
struct Received {
    message: Message,
    trailer: AuditTrailer,
}

/// `mach_msg_audit_trailer_t`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AuditTrailer {
    kind: u32,
    size: u32,
    seqno: u32,
    sender: [u32; 2],
    /// `audit_token_t`; the sender's pid is `val[5]` (`audit_token_to_pid`).
    audit: [u32; 8],
}

const _: () = assert!(std::mem::size_of::<Message>() == 44 && std::mem::offset_of!(Received, trailer) == 44);
const _: () = assert!(std::mem::size_of::<AuditTrailer>() == 52);

/// `MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) | MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT)`.
const MACH_RCV_TRAILER_AUDIT: i32 = 3 << 24;

unsafe extern "C" {
    static bootstrap_port: MachPort;
    static mach_task_self_: MachPort;
    fn bootstrap_check_in(bp: MachPort, name: *const c_char, port: *mut MachPort) -> KernReturn;
    fn bootstrap_look_up(bp: MachPort, name: *const c_char, port: *mut MachPort) -> KernReturn;
    fn mach_msg(
        msg: *mut c_void,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: MachPort,
        timeout: u32,
        notify: MachPort,
    ) -> KernReturn;
    fn mach_msg_destroy(msg: *mut Header);
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn mach_port_mod_refs(task: MachPort, name: MachPort, right: u32, delta: i32) -> KernReturn;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreateMachPort(surface: *const AnyObject) -> MachPort;
    fn IOSurfaceLookupFromMachPort(port: MachPort) -> *mut AnyObject;
}

fn task() -> MachPort {
    // SAFETY: the task's own port name, set by libSystem before main.
    unsafe { mach_task_self_ }
}

fn mach_error(op: &'static str, kr: KernReturn) -> Error {
    Error::Service { op, message: format!("kern_return_t {kr:#x}") }
}

/// A bootstrap name no other inbox of this host has.
fn fresh_name() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("dev.simee.surfaces.{}.{n}.{t:x}", std::process::id())
}

/// The process at the other end of a connected Unix socket (`LOCAL_PEERPID`): the emulator a
/// display connection reached, whose surfaces alone an inbox takes ([`SurfaceInbox::check_in`]).
pub fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: a live socket descriptor and an out-buffer of `len` bytes for the pid.
    let rc = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len) };
    (rc == 0 && pid > 0).then_some(pid)
}

/// The receive side: a port checked in under a fresh bootstrap name, which a sender looks up.
pub struct SurfaceInbox {
    port: MachPort,
    name: String,
    /// The only process whose messages are taken (by the kernel's audit token for the sender).
    sender: i32,
}

// SAFETY: a port name is valid on every thread of the task; receives on it are serialized by the
// kernel.
unsafe impl Send for SurfaceInbox {}
// SAFETY: as above.
unsafe impl Sync for SurfaceInbox {}

impl SurfaceInbox {
    /// Checks a fresh name in with launchd and takes its receive right. Only surfaces process
    /// `sender` sends are taken: the name can be found by any process of the user, so anyone else's
    /// message is dropped ([`peer_pid`] says which process the emulator is).
    pub fn check_in(sender: i32) -> Result<SurfaceInbox> {
        let name = fresh_name();
        let c = CString::new(name.clone()).map_err(|e| Error::Invalid(e.to_string()))?;
        let mut port = 0;
        // SAFETY: the task's bootstrap port, a NUL-terminated name and a live out-pointer.
        let kr = unsafe { bootstrap_check_in(bootstrap_port, c.as_ptr(), &mut port) };
        if kr != 0 || port == 0 {
            return Err(mach_error("bootstrap_check_in", kr));
        }
        Ok(SurfaceInbox { port, name, sender })
    }

    /// The bootstrap name a sender looks up (`surfaces <name>`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The next surface sent here and its sequence number, waiting up to `timeout`; `None` when
    /// none came by then. A malformed message, or one from a process other than the inbox's
    /// sender, is dropped (its rights freed) and the wait goes on.
    pub fn recv(&self, timeout: Duration) -> Result<Option<(u32, Surface)>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let mut buf = Received::default();
            let size = std::mem::size_of::<Received>() as u32;
            // SAFETY: `buf` is a writable buffer of `size` bytes, aligned for a message header; the
            // port is our receive right.
            let kr = unsafe {
                mach_msg(
                    (&raw mut buf.message.header).cast(),
                    MACH_RCV_MSG | MACH_RCV_TIMEOUT | MACH_RCV_TRAILER_AUDIT,
                    0,
                    size,
                    self.port,
                    left.as_millis().min(u128::from(u32::MAX)) as u32,
                    0,
                )
            };
            if kr == MACH_RCV_TIMED_OUT {
                return Ok(None);
            }
            // Bigger than a surface message: the kernel dropped it (no MACH_RCV_LARGE); not ours.
            if kr == MACH_RCV_TOO_LARGE {
                continue;
            }
            if kr != 0 {
                return Err(mach_error("mach_msg (receive)", kr));
            }
            let (m, t) = (buf.message, buf.trailer);
            // The kernel's word for who sent it, not the message's.
            let from_sender = t.kind == 0 && t.size as usize >= std::mem::size_of::<AuditTrailer>() && t.audit[5] as i32 == self.sender;
            let shaped = from_sender
                && m.header.id == SURFACE_MSG_ID
                && m.header.bits & MACH_MSGH_BITS_COMPLEX != 0
                && m.header.size as usize == std::mem::size_of::<Message>()
                && m.descriptors == 1
                && m.port.kind == MACH_MSG_PORT_DESCRIPTOR
                && m.port.disposition == MACH_MSG_TYPE_PORT_SEND;
            if !shaped {
                // SAFETY: a message the kernel just delivered into `buf`; this frees its rights.
                unsafe { mach_msg_destroy(&mut buf.message.header) };
                continue;
            }
            // SAFETY: a send right we were just given; the lookup returns a +1 surface or null,
            // and the right is ours to free after it.
            let surface = unsafe {
                let s = IOSurfaceLookupFromMachPort(m.port.name);
                mach_port_deallocate(task(), m.port.name);
                s
            };
            // SAFETY: a +1 IOSurfaceRef (an Objective-C object) the handle takes over.
            if let Some(s) = unsafe { Retained::from_raw(surface) } {
                return Ok(Some((m.seq, Surface::from_retained(s))));
            }
        }
    }
}

impl Drop for SurfaceInbox {
    fn drop(&mut self) {
        // SAFETY: our receive right, destroyed once; launchd drops the name with it.
        unsafe { mach_port_mod_refs(task(), self.port, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

/// The send side: a send right to an inbox, by its bootstrap name. emu's own is
/// `emu_gfxstream::metal::MachSender`; this one is for front ends and tests.
pub struct SurfaceOutbox {
    port: MachPort,
}

// SAFETY: a send right's name is valid on every thread of the task; sends are atomic.
unsafe impl Send for SurfaceOutbox {}
// SAFETY: as above.
unsafe impl Sync for SurfaceOutbox {}

impl SurfaceOutbox {
    pub fn look_up(name: &str) -> Result<SurfaceOutbox> {
        let c = CString::new(name).map_err(|e| Error::Invalid(e.to_string()))?;
        let mut port = 0;
        // SAFETY: the task's bootstrap port, a NUL-terminated name and a live out-pointer.
        let kr = unsafe { bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut port) };
        if kr != 0 || port == 0 {
            return Err(mach_error("bootstrap_look_up", kr));
        }
        Ok(SurfaceOutbox { port })
    }

    /// Sends `surface` as number `seq`, without blocking: a full queue is an error.
    pub fn send(&self, surface: &Surface, seq: u32) -> Result<()> {
        let port = surface.mach_port().ok_or_else(|| Error::Service { op: "IOSurfaceCreateMachPort", message: "no port".into() })?;
        let mut m = Message {
            header: Header {
                bits: MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX,
                size: std::mem::size_of::<Message>() as u32,
                remote: self.port,
                id: SURFACE_MSG_ID,
                ..Header::default()
            },
            descriptors: 1,
            port: PortDescriptor {
                name: port,
                disposition: MACH_MSG_TYPE_MOVE_SEND as u8,
                kind: MACH_MSG_PORT_DESCRIPTOR,
                ..PortDescriptor::default()
            },
            seq,
        };
        // SAFETY: a well-formed message of the size given, to our send right.
        let kr = unsafe { mach_msg((&raw mut m.header).cast(), MACH_SEND_MSG | MACH_SEND_TIMEOUT, m.header.size, 0, 0, 0, 0) };
        if kr != 0 {
            // Not sent: the moved right is still ours (returned under its name), so free it.
            // SAFETY: a right this task holds.
            unsafe { mach_port_deallocate(task(), m.port.name) };
            // Copied in and then handed back (a full queue times out): the destination came back
            // too, one more reference on our right to the inbox, dropped here.
            if sim_core::stream::mach_send_pseudo_received(kr) && m.header.remote != 0 {
                // SAFETY: the reference the pseudo-receive just gave this task.
                unsafe { mach_port_deallocate(task(), m.header.remote) };
            }
            return Err(mach_error("mach_msg (send)", kr));
        }
        Ok(())
    }
}

impl Drop for SurfaceOutbox {
    fn drop(&mut self) {
        // SAFETY: our send right, released once.
        unsafe { mach_port_deallocate(task(), self.port) };
    }
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
    fn IOSurfaceCreate(properties: *const c_void) -> *mut AnyObject;
    fn IOSurfaceGetID(surface: *const AnyObject) -> u32;
}

impl Surface {
    /// A new BGRA surface of `width` x `height` (rows 64-byte aligned), for senders and tests.
    pub fn new_bgra(width: u32, height: u32) -> Result<Surface> {
        let stride = (i64::from(width) * 4 + 63) & !63;
        let number = |v: i64| {
            // SAFETY: kCFNumberSInt64Type (4) reads an i64 from a live pointer.
            unsafe { CFNumberCreate(std::ptr::null(), 4, (&raw const v).cast()) }
        };
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
                (kIOSurfaceBytesPerRow, stride),
                (kIOSurfacePixelFormat, i64::from(u32::from_be_bytes(*b"BGRA"))),
            ] {
                let n = number(v);
                CFDictionarySetValue(dict, k, n);
                CFRelease(n);
            }
            let s = IOSurfaceCreate(dict);
            CFRelease(dict);
            s
        };
        // SAFETY: IOSurfaceCreate returns a +1 surface or null.
        let s = unsafe { Retained::from_raw(surface) }.ok_or_else(|| Error::Service { op: "IOSurfaceCreate", message: "null".into() })?;
        Ok(Surface::from_retained(s))
    }

    /// A new Mach send right for the surface (`IOSurfaceCreateMachPort`), owned by the caller.
    pub fn mach_port(&self) -> Option<u32> {
        // SAFETY: a live surface; the result is a new send right (0 on failure).
        Some(unsafe { IOSurfaceCreateMachPort(self.as_raw().cast()) }).filter(|&p| p != 0)
    }

    /// The surface's system-wide ID (`IOSurfaceGetID`): the same surface has the same ID in every
    /// process.
    pub fn id(&self) -> u32 {
        // SAFETY: a live, retained IOSurface.
        unsafe { IOSurfaceGetID(self.as_raw().cast()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> i32 {
        std::process::id() as i32
    }

    unsafe extern "C" {
        fn mach_port_get_refs(task: MachPort, name: MachPort, right: u32, refs: *mut u32) -> KernReturn;
    }

    /// User references this task holds on send right `port`.
    fn send_refs(port: MachPort) -> u32 {
        let mut refs = 0;
        // SAFETY: the task's own port, a name it holds and a live out-pointer; MACH_PORT_RIGHT_SEND
        // is 0.
        let kr = unsafe { mach_port_get_refs(task(), port, 0, &mut refs) };
        assert_eq!(kr, 0, "mach_port_get_refs");
        refs
    }

    #[test]
    fn a_refused_send_leaves_the_outboxs_right_to_the_inbox_as_it_was() {
        let inbox = SurfaceInbox::check_in(me()).expect("check in");
        let outbox = SurfaceOutbox::look_up(inbox.name()).expect("look up");
        let s = Surface::new_bgra(4, 4).expect("surface");
        let before = send_refs(outbox.port);
        let sent = (0..64).take_while(|&i| outbox.send(&s, i).is_ok()).count();
        assert!(sent < 64, "the queue fills");
        for i in 0..3 {
            assert!(outbox.send(&s, 100 + i).is_err(), "still full");
        }
        assert_eq!(send_refs(outbox.port), before, "a timed-out send hands the destination back: dropped again");
    }

    #[test]
    fn an_inbox_takes_surfaces_only_from_the_sender_it_was_made_for() {
        let s = Surface::new_bgra(4, 4).expect("surface");
        // Made for another process: ours is dropped, rights and all, and nothing is left queued.
        let theirs = SurfaceInbox::check_in(me() + 1).expect("check in");
        let outbox = SurfaceOutbox::look_up(theirs.name()).expect("look up");
        outbox.send(&s, 1).expect("send");
        assert!(theirs.recv(Duration::ZERO).expect("recv").is_none(), "not from its sender");
        // Made for this one: taken.
        let ours = SurfaceInbox::check_in(me()).expect("check in");
        let outbox = SurfaceOutbox::look_up(ours.name()).expect("look up");
        outbox.send(&s, 2).expect("send");
        assert_eq!(ours.recv(Duration::ZERO).expect("recv").map(|(seq, got)| (seq, got.id())), Some((2, s.id())));
    }

    #[test]
    fn a_connected_sockets_peer_is_found_by_pid() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("pair");
        assert_eq!(peer_pid(&a), Some(me()));
    }

    #[test]
    fn surfaces_cross_a_checked_in_port_by_name() {
        let inbox = SurfaceInbox::check_in(me()).expect("check in");
        let outbox = SurfaceOutbox::look_up(inbox.name()).expect("look up");
        assert!(inbox.recv(Duration::from_millis(1)).expect("recv").is_none(), "nothing sent yet");
        let a = Surface::new_bgra(64, 32).expect("surface");
        let b = Surface::new_bgra(8, 8).expect("surface");
        outbox.send(&a, 1).expect("send a");
        outbox.send(&b, 2).expect("send b");
        // In order, the same surfaces (by ID), at their sizes.
        let (seq, got) = inbox.recv(Duration::from_secs(1)).expect("recv").expect("a");
        assert_eq!((seq, got.id(), got.size()), (1, a.id(), (64, 32)));
        let (seq, got) = inbox.recv(Duration::from_secs(1)).expect("recv").expect("b");
        assert_eq!((seq, got.id(), got.size()), (2, b.id(), (8, 8)));
        // A dropped inbox takes its name with it: a sender can no longer find it.
        let name = inbox.name().to_owned();
        drop(inbox);
        assert!(outbox.send(&a, 3).is_err(), "the port is dead");
        // launchd releases the name a moment after the port dies, not in the same instant: poll
        // for it, up to a deadline that only a name launchd never releases outlasts.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while SurfaceOutbox::look_up(&name).is_ok() {
            assert!(std::time::Instant::now() < deadline, "the name went with its port");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_full_inbox_refuses_rather_than_blocking() {
        let inbox = SurfaceInbox::check_in(me()).expect("check in");
        let outbox = SurfaceOutbox::look_up(inbox.name()).expect("look up");
        let s = Surface::new_bgra(4, 4).expect("surface");
        let sent = (0..64).take_while(|&i| outbox.send(&s, i).is_ok()).count();
        assert!(sent < 64, "a port queues a few messages, not 64");
        // Refused because it would have to wait (a zero timeout), not queued or blocked on.
        let refused = outbox.send(&s, 64).expect_err("full").to_string();
        assert!(refused.contains("0x10000004"), "MACH_SEND_TIMED_OUT: {refused}");
        // The ones queued still arrive.
        for i in 0..sent as u32 {
            assert_eq!(inbox.recv(Duration::from_secs(1)).expect("recv").map(|(seq, _)| seq), Some(i));
        }
    }
}
