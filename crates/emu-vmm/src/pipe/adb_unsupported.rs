//! The adb bridge's names where the bridge itself doesn't exist (Windows): the same types, none
//! of which can be built, and a `bind` that fails with [`std::io::ErrorKind::Unsupported`].
//!
//! The bridge (`adb.rs`) waits on its listener and on every client with `poll(2)` and wakes
//! itself through a socket pair; Windows needs another wait (#248). `announce_port` doesn't wait
//! on anything, so it works.
use std::convert::Infallible;
use std::io::Write;
use std::net::{Ipv4Addr, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use super::{PipeCtx, PipeHandler, PipeService, WorkerEvent};

pub const ADBD_VSOCK_PORT: u32 = 5555;

fn unsupported<T>() -> std::io::Result<T> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "the adb bridge is not available on this host yet"))
}

/// The adb port, bound once per emulator. It can't be bound here.
pub struct AdbListener {
    pub port: u16,
    never: Infallible,
}

impl AdbListener {
    pub fn bind(_port: u16) -> std::io::Result<AdbListener> {
        unsupported()
    }

    pub fn attach(self: &Arc<Self>, _vsock: Sender<WorkerEvent>) -> AdbService {
        match self.never {}
    }
}

/// One machine's end of the adb bridge. It can't be built here.
pub struct AdbService {
    pub port: u16,
    never: Infallible,
}

impl AdbService {
    pub fn bind(_port: u16, _vsock: Sender<WorkerEvent>) -> std::io::Result<AdbService> {
        unsupported()
    }

    /// Tells a running adb server about us (`host:emulator:<port>`), so `adb devices` lists
    /// `emulator-<port-1>` right away instead of on its next scan.
    pub fn announce_port(port: u16) {
        let server_port: u16 = std::env::var("ANDROID_ADB_SERVER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5037);
        if let Ok(mut s) = TcpStream::connect((Ipv4Addr::LOCALHOST, server_port)) {
            let req = format!("host:emulator:{port}");
            let _ = s.write_all(format!("{:04x}{req}", req.len()).as_bytes());
        }
    }
}

impl PipeService for AdbService {
    fn open(&self, _name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        match self.never {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_says_it_is_unsupported() {
        let (events, _rx) = std::sync::mpsc::channel();
        assert_eq!(AdbListener::bind(0).err().map(|e| e.kind()), Some(std::io::ErrorKind::Unsupported));
        assert_eq!(AdbService::bind(0, events).err().map(|e| e.kind()), Some(std::io::ErrorKind::Unsupported));
    }

    #[test]
    fn announcing_a_port_with_no_adb_server_is_harmless() {
        // Nothing listens on the adb server's port in a test run (or if it does, it is told).
        AdbService::announce_port(5554);
    }
}
