//! Host services the guest reaches through "qemu pipes".
//!
//! On current system images the guest's pipe library connects a vsock stream to the host
//! (CID 2, port 5000) and sends `pipe:<name>\0`; everything after that belongs to the named
//! service. `qemud:adb` is adbd's transport; others (boot properties, sensors, ...) follow.
#[cfg(unix)]
pub mod adb;
#[cfg(not(unix))]
#[path = "adb_unsupported.rs"]
pub mod adb;
pub mod camera;
pub mod car;
pub mod clipboard;
pub mod fingerprint;
pub mod gps;
pub mod gsm;
pub mod logcat;
pub mod misc;
pub mod opengles;
pub mod qemud;
pub mod sensors;
pub mod xr;

use std::net::TcpStream;
use std::sync::mpsc::Sender;

/// Messages from host-side threads to the vsock worker, addressed to one pipe connection.
pub enum HostMsg {
    Data(Vec<u8>),
    Close,
    /// An accepted host TCP client for this connection (adb).
    Attach(TcpStream),
}

/// Builds the host end of a host-initiated vsock connection once it has an id.
pub type MakeHandler = Box<dyn FnOnce(PipeCtx) -> Box<dyn PipeHandler> + Send>;

pub enum WorkerEvent {
    /// The guest notified a virtqueue.
    Kick,
    Host {
        conn: u64,
        msg: HostMsg,
    },
    /// Open a stream to `guest_port` in the guest. `refused` runs if nothing listens there.
    Connect {
        guest_port: u32,
        make: MakeHandler,
        refused: Box<dyn FnOnce() + Send>,
    },
    /// The host side of `conn` delivered guest bytes it was holding ([`PipeHandler::backlog`]
    /// shrank), so the guest may get its credit back.
    Drained {
        conn: u64,
    },
    Stop,
}

/// Handed to a service when a guest opens a pipe; lets host threads talk to that connection.
#[derive(Clone)]
pub struct PipeCtx {
    pub conn: u64,
    pub events: Sender<WorkerEvent>,
}

impl PipeCtx {
    pub fn send(&self, msg: HostMsg) -> bool {
        self.events.send(WorkerEvent::Host { conn: self.conn, msg }).is_ok()
    }

    /// Reports that [`PipeHandler::backlog`] shrank.
    pub fn drained(&self) -> bool {
        self.events.send(WorkerEvent::Drained { conn: self.conn }).is_ok()
    }
}

/// One open pipe. Bytes for the guest are appended to `to_guest`; returning false closes it.
///
/// Handlers run on the transport's thread (the vsock worker serves every pipe), so they must
/// never block: a handler whose host side can be slow (a socket) queues guest bytes, reports
/// them as [`backlog`](PipeHandler::backlog) and calls [`PipeCtx::drained`] as they go out.
/// vsock returns credit to the guest only for bytes that left the backlog, so a slow consumer
/// slows the guest's sender on that connection alone, and the backlog never exceeds the
/// receive buffer vsock advertises.
pub trait PipeHandler: Send {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool;
    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool;
    fn closed(&mut self) {}
    /// Guest bytes accepted but not yet delivered to the host side.
    fn backlog(&self) -> usize {
        0
    }
    /// The guest took `n` more of the bytes this pipe queued for it (a host producer can bound
    /// what it has in flight by this).
    fn guest_took(&mut self, _n: usize) {}
    /// True while the host side can take no more guest bytes (a full render queue). The
    /// goldfish pipe then answers writes with "try again" and wakes the guest's writer on the
    /// handler's next [`PipeCtx::drained`] that finds it unblocked.
    fn write_blocked(&self) -> bool {
        false
    }
}

pub trait PipeService: Send + Sync {
    /// Opens `name` (e.g. `qemud:adb:5555`), or returns None for names it doesn't serve.
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>>;
}

/// Routes pipe names to services by prefix.
#[derive(Default)]
pub struct PipeRegistry {
    services: Vec<(String, Box<dyn PipeService>)>,
}

impl PipeRegistry {
    pub fn register(&mut self, prefix: &str, svc: Box<dyn PipeService>) {
        self.services.push((prefix.to_string(), svc));
    }

    pub fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        self.services.iter().filter(|(p, _)| name.starts_with(p.as_str())).find_map(|(_, s)| s.open(name, ctx.clone()))
    }
}
