//! `qemud:fingerprintlisten`: the goldfish fingerprint HAL.
//!
//! The HAL sends a qemud-framed `listen`, then waits for `on:<id>` (a finger with
//! that enrolled id touches the sensor) or `off` (lifted). The console's `finger touch <id>` /
//! `finger remove` drive it through [`Fingerprint`].
//! The camera HAL's `qemud:camera` is [`super::camera`].
use std::sync::{Arc, Mutex};

use super::qemud::{frame, take_frames};
use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

#[derive(Default)]
pub struct Fingerprint {
    listeners: Mutex<Vec<PipeCtx>>,
}

impl Fingerprint {
    fn broadcast(&self, msg: &str) -> bool {
        let mut out = Vec::new();
        frame(&mut out, msg.as_bytes());
        let mut l = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        l.retain(|c| c.send(HostMsg::Data(out.clone())));
        !l.is_empty()
    }

    /// A finger with enrolled id `id` (> 0) touches the sensor. False if no HAL is listening.
    pub fn touch(&self, id: u32) -> bool {
        self.broadcast(&format!("on:{id}"))
    }

    pub fn remove(&self) -> bool {
        self.broadcast("off")
    }
}

pub struct FingerprintService {
    pub fingerprint: Arc<Fingerprint>,
}

impl PipeService for FingerprintService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "qemud:fingerprintlisten" {
            return None;
        }
        let conn = ctx.conn;
        self.fingerprint.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(ctx);
        Some(Box::new(FingerprintPipe { fingerprint: self.fingerprint.clone(), conn, buf: Vec::new() }))
    }
}

struct FingerprintPipe {
    fingerprint: Arc<Fingerprint>,
    conn: u64,
    buf: Vec<u8>,
}

impl PipeHandler for FingerprintPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        // Only `listen` is ever sent; nothing to answer.
        self.buf.extend_from_slice(data);
        let _ = take_frames(&mut self.buf);
        true
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) => {
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn closed(&mut self) {
        let conn = self.conn;
        self.fingerprint.listeners.lock().unwrap_or_else(|e| e.into_inner()).retain(|l| l.conn != conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_touch_reaches_listeners() {
        let fp = Arc::new(Fingerprint::default());
        assert!(!fp.touch(1), "nobody listening");
        let (tx, rx) = std::sync::mpsc::channel();
        let svc = FingerprintService { fingerprint: fp.clone() };
        let mut p = svc.open("qemud:fingerprintlisten", PipeCtx { conn: 3, events: tx }).expect("open");
        let mut req = Vec::new();
        frame(&mut req, b"listen");
        let mut out = Vec::new();
        assert!(p.guest_data(&req, &mut out));
        assert!(out.is_empty());
        assert!(fp.touch(2));
        assert!(fp.remove());
        let mut got = Vec::new();
        for _ in 0..2 {
            let crate::pipe::WorkerEvent::Host { msg: HostMsg::Data(d), .. } = rx.try_recv().expect("msg") else { panic!() };
            got.extend_from_slice(&d);
        }
        assert_eq!(take_frames(&mut got), vec![b"on:2".to_vec(), b"off".to_vec()]);
        p.closed();
        assert!(!fp.touch(1));
    }
}
