//! virtio-vsock (stream sockets only), terminating guest connections in host pipe services.
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use emu_hvf::GuestRam;

use super::mmio::F_RING_EVENT_IDX;
use super::queue::{Chain, Queue};
use super::{Activation, INT_USED_RING, Interrupt, VirtioDevice};
use crate::pipe::{PipeCtx, PipeHandler, PipeRegistry, WorkerEvent};

pub const ID_VSOCK: u32 = 19;
pub const HOST_CID: u64 = 2;
/// Where the guest's qemu-pipe library connects.
pub const PIPE_PORT: u32 = 5000;

const HDR: usize = 44;
const TYPE_STREAM: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

const RXQ: usize = 0;
const TXQ: usize = 1;
/// Receive buffer we advertise per connection.
const BUF_ALLOC: u32 = 256 * 1024;
const MAX_PKT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hdr {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    typ: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Hdr {
    fn parse(b: &[u8]) -> Option<Hdr> {
        if b.len() < HDR {
            return None;
        }
        let u64_ = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        let u32_ = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or_default());
        let u16_ = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Some(Hdr {
            src_cid: u64_(0),
            dst_cid: u64_(8),
            src_port: u32_(16),
            dst_port: u32_(20),
            len: u32_(24),
            typ: u16_(28),
            op: u16_(30),
            flags: u32_(32),
            buf_alloc: u32_(36),
            fwd_cnt: u32_(40),
        })
    }

    fn encode(&self) -> [u8; HDR] {
        let mut b = [0u8; HDR];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.typ.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

pub struct Vsock {
    guest_cid: u64,
    registry: Arc<PipeRegistry>,
    events: Sender<WorkerEvent>,
    rx: Option<Receiver<WorkerEvent>>,
    worker: Option<JoinHandle<Receiver<WorkerEvent>>>,
}

/// The vsock worker's event channel, created first so host services can hold a sender.
pub fn event_channel() -> (Sender<WorkerEvent>, Receiver<WorkerEvent>) {
    channel()
}

impl Vsock {
    pub fn new(guest_cid: u64, registry: Arc<PipeRegistry>, events: Sender<WorkerEvent>, rx: Receiver<WorkerEvent>) -> Vsock {
        Vsock { guest_cid, registry, events, rx: Some(rx), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.events.send(WorkerEvent::Stop);
            if let Ok(rx) = h.join() {
                // Keep the receiver: host threads hold senders for the device's lifetime.
                while rx.try_recv().is_ok() {}
                self.rx = Some(rx);
            }
        }
    }
}

impl Drop for Vsock {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Vsock {
    fn device_id(&self) -> u32 {
        ID_VSOCK
    }
    /// Event-idx: the worker drains tx with kicks off and the guest kicks rx only once the
    /// device has taken every buffer, so most packets cost the guest no MMIO exit (Linux's
    /// driver honours it on all three queues; the event queue, which the device never uses,
    /// costs one kick at probe either way).
    fn features(&self) -> u64 {
        F_RING_EVENT_IDX
    }
    fn num_queues(&self) -> usize {
        3
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cfg = self.guest_cid.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let rxq = act.queues.get(RXQ).copied().flatten().ok_or("vsock rx queue not enabled")?;
        let txq = act.queues.get(TXQ).copied().flatten().ok_or("vsock tx queue not enabled")?;
        self.stop_worker();
        let rx = self.rx.take().ok_or("vsock worker receiver missing")?;
        let event_idx = act.features & F_RING_EVENT_IDX != 0;
        let (rxq, txq) = (Queue::new(rxq, event_idx), Queue::new(txq, event_idx));
        let mut w = Worker::new(act.mem, act.irq, rxq, txq, self.guest_cid, self.registry.clone(), self.events.clone());
        let handle = std::thread::Builder::new()
            .name("vsock".into())
            .spawn(move || {
                crate::device_thread();
                w.run(&rx);
                w.close_all();
                rx
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some(handle);
        Ok(())
    }
    fn notify(&mut self, _queue: u16) {
        let _ = self.events.send(WorkerEvent::Kick);
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

/// (guest port, host port)
type Key = (u32, u32);

struct Conn {
    id: u64,
    /// False until the guest answers a host-initiated REQUEST.
    connected: bool,
    /// For host-initiated connections still waiting on the guest: what to run on refusal, and
    /// the handler to build on acceptance (so host I/O starts only once the guest listens).
    refused: Option<Box<dyn FnOnce() + Send>>,
    pending: Option<crate::pipe::MakeHandler>,
    /// Collecting `pipe:<name>\0` until the handler exists.
    name_buf: Option<Vec<u8>>,
    handler: Option<Box<dyn PipeHandler>>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Payload bytes sent to the guest.
    tx_cnt: u32,
    /// Payload bytes received from the guest.
    rx_cnt: u32,
    /// Of those, the bytes the host side has taken: all but the handler's backlog. The guest's
    /// credit is counted from this, so a slow host side withholds it.
    fwd_cnt: u32,
    /// The `fwd_cnt` last put in a packet to the guest.
    advertised_fwd: u32,
    /// The handler held a backlog at the last credit check.
    held: bool,
    out: VecDeque<u8>,
    closing: bool,
    /// Queued in `Worker::ready`.
    ready: bool,
    /// A credit update waits in `Worker::control`.
    update_queued: bool,
}

impl Conn {
    fn new(id: u64, connected: bool) -> Conn {
        Conn {
            id,
            connected,
            refused: None,
            pending: None,
            name_buf: None,
            handler: None,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            rx_cnt: 0,
            fwd_cnt: 0,
            advertised_fwd: 0,
            held: false,
            out: VecDeque::new(),
            closing: false,
            ready: false,
            update_queued: false,
        }
    }

    fn credit(&self) -> usize {
        self.peer_buf_alloc.saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt)) as usize
    }

    /// Has data for the guest and the guest has room for it.
    fn sendable(&self) -> bool {
        self.connected && !self.out.is_empty() && self.credit() > 0
    }
}

/// Puts `key` at the back of the ready queue if it has something to send and isn't queued.
fn schedule(ready: &mut VecDeque<Key>, key: Key, c: &mut Conn) {
    if !c.ready && c.sendable() {
        c.ready = true;
        ready.push_back(key);
    }
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    rxq: Queue,
    txq: Queue,
    guest_cid: u64,
    registry: Arc<PipeRegistry>,
    events: Sender<WorkerEvent>,
    conns: HashMap<Key, Conn>,
    ids: HashMap<u64, Key>,
    next_id: u64,
    next_host_port: u32,
    /// Header-only packets (responses, resets, credit updates) waiting for rx buffers.
    control: VecDeque<Hdr>,
    /// Connections with data the guest has room for, served a buffer each in turn.
    ready: VecDeque<Key>,
    /// Connections whose host side finished, closed once their data drains.
    closing: Vec<Key>,
    /// Tx packets wait until the guest has taken the replies queued in `control`.
    tx_throttled: bool,
    /// The guest packet being handled, reused across packets.
    pkt: Vec<u8>,
    /// A handler's replies to guest data, reused across packets.
    scratch: Vec<u8>,
    /// The tx chain being handled, reused across packets.
    tx_chain: Chain,
    /// A queue got used entries the driver wants an interrupt for: raised once per pass, so a
    /// tx completion and the reply it caused cost the guest one interrupt, not two.
    irq_due: bool,
}

impl Worker {
    fn new(
        mem: Arc<GuestRam>,
        irq: Arc<Interrupt>,
        rxq: Queue,
        txq: Queue,
        guest_cid: u64,
        registry: Arc<PipeRegistry>,
        events: Sender<WorkerEvent>,
    ) -> Worker {
        Worker {
            mem,
            irq,
            rxq,
            txq,
            guest_cid,
            registry,
            events,
            conns: HashMap::new(),
            ids: HashMap::new(),
            next_id: 1,
            next_host_port: 50_000,
            control: VecDeque::new(),
            ready: VecDeque::new(),
            closing: Vec::new(),
            tx_throttled: false,
            pkt: Vec::new(),
            scratch: Vec::new(),
            tx_chain: Chain::default(),
            irq_due: false,
        }
    }

    fn run(&mut self, rx: &Receiver<WorkerEvent>) {
        while let Ok(ev) = rx.recv() {
            let mut ev = Some(ev);
            // Drain everything queued before touching the rings again.
            while let Some(e) = ev.take() {
                match e {
                    WorkerEvent::Stop => return,
                    WorkerEvent::Kick => self.process_tx(),
                    WorkerEvent::Host { conn, msg } => self.host_msg(conn, msg),
                    WorkerEvent::Connect { guest_port, make, refused } => self.connect(guest_port, make, refused),
                    WorkerEvent::Drained { conn } => {
                        if let Some(&key) = self.ids.get(&conn) {
                            self.refresh_credit(key);
                        }
                    }
                }
                ev = rx.try_recv().ok();
            }
            self.flush();
            // The guest took the replies that held its tx packets back: take those now (it
            // won't kick the tx queue again for packets it already published).
            while self.tx_throttled && self.control.len() < self.control_limit() {
                self.process_tx();
                self.flush();
            }
            if std::mem::take(&mut self.irq_due) {
                self.irq.raise(INT_USED_RING);
            }
        }
    }

    /// Most header-only replies queued before tx packets wait for the guest to take them (as
    /// vhost-vsock does), so a guest that sends without posting rx buffers can't grow the
    /// queue without bound.
    fn control_limit(&self) -> usize {
        usize::from(self.rxq.size()).max(1)
    }

    fn connect(&mut self, guest_port: u32, make: crate::pipe::MakeHandler, refused: Box<dyn FnOnce() + Send>) {
        let host_port = loop {
            let p = self.next_host_port;
            self.next_host_port = if p >= 60_000 { 50_000 } else { p + 1 };
            if !self.conns.contains_key(&(guest_port, p)) {
                break p;
            }
        };
        let key = (guest_port, host_port);
        let id = self.next_id;
        self.next_id += 1;
        let mut conn = Conn::new(id, false);
        conn.refused = Some(refused);
        conn.pending = Some(make);
        self.control.push_back(reply(self.guest_cid, key, OP_REQUEST, 0, 0));
        self.conns.insert(key, conn);
        self.ids.insert(id, key);
    }

    /// Takes every tx packet the guest published, with kicks off while it does
    /// ([`Queue::pop_draining`]): packets the guest sends meanwhile cost it no exit.
    fn process_tx(&mut self) {
        self.tx_throttled = false;
        let mut done = false;
        let mut chain = std::mem::take(&mut self.tx_chain);
        loop {
            if self.control.len() >= self.control_limit() {
                self.tx_throttled = true;
                // Kicks back on before leaving packets in the ring; the run loop comes back
                // for them once the guest takes the replies.
                self.txq.enable_notify(&self.mem);
                break;
            }
            match self.txq.pop_draining(&self.mem, &mut chain) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    eprintln!("emu: vsock tx: {e}");
                    break;
                }
            }
            // One packet buffer for the worker's life, not an allocation per packet.
            let mut pkt = std::mem::take(&mut self.pkt);
            if chain.read_all_into(&self.mem, HDR + MAX_PKT, &mut pkt).is_ok()
                && let Some(h) = Hdr::parse(&pkt)
            {
                let end = (HDR + h.len as usize).min(pkt.len());
                self.guest_packet(h, &pkt[HDR..end]);
            }
            self.pkt = pkt;
            let _ = self.txq.add_used(&self.mem, chain.head, 0);
            done = true;
        }
        self.tx_chain = chain;
        if done && self.txq.should_notify(&self.mem) {
            self.irq_due = true;
        }
    }

    fn guest_packet(&mut self, h: Hdr, payload: &[u8]) {
        if h.dst_cid != HOST_CID || h.typ != TYPE_STREAM {
            if h.op != OP_RST {
                self.control.push_back(reply(self.guest_cid, (h.src_port, h.dst_port), OP_RST, 0, 0));
            }
            return;
        }
        let key = (h.src_port, h.dst_port);
        if h.op == OP_REQUEST {
            if h.dst_port != PIPE_PORT || self.conns.contains_key(&key) {
                self.control.push_back(reply(self.guest_cid, key, OP_RST, 0, 0));
                return;
            }
            let id = self.next_id;
            self.next_id += 1;
            let mut conn = Conn::new(id, true);
            conn.name_buf = Some(Vec::new());
            (conn.peer_buf_alloc, conn.peer_fwd_cnt) = (h.buf_alloc, h.fwd_cnt);
            self.control.push_back(reply(self.guest_cid, key, OP_RESPONSE, 0, conn.fwd_cnt));
            self.conns.insert(key, conn);
            self.ids.insert(id, key);
            return;
        }
        let Some(conn) = self.conns.get_mut(&key) else {
            if h.op != OP_RST {
                self.control.push_back(reply(self.guest_cid, key, OP_RST, 0, 0));
            }
            return;
        };
        conn.peer_buf_alloc = h.buf_alloc;
        conn.peer_fwd_cnt = h.fwd_cnt;
        // Every header carries the guest's credit: data waiting for it may go now.
        schedule(&mut self.ready, key, conn);
        match h.op {
            OP_RW => {
                let len = payload.len() as u32;
                // A guest may only send what it was told it may; one that doesn't is reset
                // (as Linux and Firecracker do) rather than buffered without bound.
                if conn.rx_cnt.wrapping_add(len).wrapping_sub(conn.advertised_fwd) > BUF_ALLOC {
                    eprintln!("emu: vsock: guest port {} sent past its credit; resetting", key.0);
                    self.close(key, true);
                    return;
                }
                conn.rx_cnt = conn.rx_cnt.wrapping_add(len);
                if !self.deliver(key, payload) {
                    self.close(key, true);
                    return;
                }
                self.refresh_credit(key);
            }
            OP_CREDIT_REQUEST => {
                if !conn.update_queued {
                    conn.update_queued = true;
                    self.control.push_back(reply(self.guest_cid, key, OP_CREDIT_UPDATE, 0, conn.fwd_cnt));
                }
            }
            OP_CREDIT_UPDATE => {}
            OP_RESPONSE => {
                conn.connected = true;
                conn.refused = None;
                if let Some(make) = conn.pending.take() {
                    conn.handler = Some(make(PipeCtx { conn: conn.id, events: self.events.clone() }));
                }
            }
            OP_SHUTDOWN => self.close(key, true),
            OP_RST => {
                if let Some(refused) = conn.refused.take() {
                    // Nothing listening: drop silently, let the requester retry.
                    if let Some(c) = self.conns.remove(&key) {
                        self.ids.remove(&c.id);
                    }
                    refused();
                } else {
                    self.close(key, false);
                }
            }
            _ => {}
        }
    }

    /// Feeds guest bytes to the connection's pipe (opening it once the name is complete).
    fn deliver(&mut self, key: Key, mut data: &[u8]) -> bool {
        let Some(conn) = self.conns.get_mut(&key) else { return false };
        if let Some(name_buf) = conn.name_buf.as_mut() {
            match data.iter().position(|&b| b == 0) {
                None => {
                    name_buf.extend_from_slice(data);
                    return name_buf.len() < 256;
                }
                Some(nul) => {
                    name_buf.extend_from_slice(&data[..nul]);
                    data = &data[nul + 1..];
                    let raw = conn.name_buf.take().unwrap_or_default();
                    let full = String::from_utf8_lossy(&raw).into_owned();
                    let name = full.strip_prefix("pipe:").unwrap_or(&full).to_string();
                    let ctx = PipeCtx { conn: conn.id, events: self.events.clone() };
                    match self.registry.open(&name, ctx) {
                        Some(h) => conn.handler = Some(h),
                        None => {
                            eprintln!("emu: guest opened unknown pipe '{name}'");
                            return false;
                        }
                    }
                }
            }
        }
        if data.is_empty() {
            return true;
        }
        let Some(h) = conn.handler.as_mut() else { return false };
        self.scratch.clear();
        let keep = h.guest_data(data, &mut self.scratch);
        conn.out.extend(&self.scratch);
        schedule(&mut self.ready, key, conn);
        keep
    }

    /// Recounts what the host side has taken of the guest's bytes, and queues a credit update
    /// when the guest should hear about it.
    fn refresh_credit(&mut self, key: Key) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        let backlog = c.handler.as_ref().map_or(0, |h| h.backlog());
        // Never backwards, whatever a handler reports.
        let backlog = (backlog as u64).min(u64::from(c.rx_cnt.wrapping_sub(c.fwd_cnt))) as u32;
        c.fwd_cnt = c.rx_cnt.wrapping_sub(backlog);
        let freed = c.fwd_cnt.wrapping_sub(c.advertised_fwd);
        let drained = c.held && backlog == 0;
        c.held = backlog > 0;
        // Linux guests never ask for credit (a blocked sender waits for any packet with a newer
        // fwd_cnt), so say so unasked: once half the buffer is free, and as soon as a backlog
        // that held the guest back is gone.
        if freed > 0 && (freed >= BUF_ALLOC / 2 || drained) && !c.update_queued {
            c.update_queued = true;
            self.control.push_back(reply(self.guest_cid, key, OP_CREDIT_UPDATE, 0, c.fwd_cnt));
        }
    }

    fn host_msg(&mut self, id: u64, msg: crate::pipe::HostMsg) {
        let Some(&key) = self.ids.get(&id) else { return };
        let Some(conn) = self.conns.get_mut(&key) else { return };
        let Some(h) = conn.handler.as_mut() else { return };
        let mut to_guest = Vec::new();
        let keep = h.host_msg(msg, &mut to_guest);
        if conn.out.is_empty() {
            conn.out = to_guest.into(); // takes the buffer over, no copy
        } else {
            conn.out.extend(to_guest);
        }
        schedule(&mut self.ready, key, conn);
        if !keep && !conn.closing {
            conn.closing = true;
            self.closing.push(key);
        }
    }

    fn close(&mut self, key: Key, send_rst: bool) {
        if let Some(mut c) = self.conns.remove(&key) {
            self.ids.remove(&c.id);
            if c.ready {
                self.ready.retain(|k| *k != key);
            }
            if let Some(h) = c.handler.as_mut() {
                h.closed();
            }
            if send_rst {
                self.control.push_back(reply(self.guest_cid, key, OP_RST, 0, 0));
            }
        }
    }

    fn close_all(&mut self) {
        let keys: Vec<Key> = self.conns.keys().copied().collect();
        for k in keys {
            self.close(k, false);
        }
    }

    /// Moves control packets and pending data into guest rx buffers.
    fn flush(&mut self) {
        let mut used = false;
        loop {
            let starved = self.fill_rx(&mut used);
            // Connections whose data just drained queue their resets: those go now too, not
            // whenever the guest next kicks (under event-idx it kicks only once out of buffers).
            if !self.reap_closing() || starved {
                break;
            }
        }
        if used && self.rxq.should_notify(&self.mem) {
            self.irq_due = true;
        }
    }

    /// Moves queued rx work into guest buffers until either runs out; true if buffers did.
    ///
    /// The device needs the guest's rx refill kicks only once it has run out of buffers, so the
    /// queue stays quiet (no kicks asked for, no `avail_event` store and fence per buffer)
    /// except while starved: then it asks for the next buffer and rechecks, so a refill
    /// published in between is used now and one published after it kicks.
    fn fill_rx(&mut self, used: &mut bool) -> bool {
        while let Some(work) = self.next_rx_work() {
            self.rxq.disable_notify(&self.mem);
            let chain = loop {
                match self.rxq.pop(&self.mem) {
                    Ok(Some(c)) => break Some(c),
                    Ok(None) if self.rxq.enable_notify(&self.mem) => self.rxq.disable_notify(&self.mem),
                    Ok(None) => break None,
                    Err(e) => {
                        eprintln!("emu: vsock rx: {e}");
                        self.rxq.enable_notify(&self.mem);
                        break None;
                    }
                }
            };
            let Some(chain) = chain else {
                self.unpop(work);
                return true;
            };
            let cap: usize = chain.writable().map(|d| d.len as usize).sum::<usize>().saturating_sub(HDR);
            let written = match work {
                RxWork::Control(mut h) => {
                    if h.op == OP_CREDIT_UPDATE
                        && let Some(c) = self.conns.get_mut(&(h.dst_port, h.src_port))
                    {
                        // As of now, not as of when it was queued.
                        h.fwd_cnt = c.fwd_cnt;
                        c.advertised_fwd = c.fwd_cnt;
                        c.update_queued = false;
                    }
                    chain.write_all(&self.mem, &h.encode()).unwrap_or(0)
                }
                RxWork::Data(key) => {
                    let Some(c) = self.conns.get_mut(&key) else {
                        let _ = self.rxq.add_used(&self.mem, chain.head, 0);
                        *used = true;
                        continue;
                    };
                    let n = c.out.len().min(c.credit()).min(cap).min(MAX_PKT);
                    c.tx_cnt = c.tx_cnt.wrapping_add(n as u32);
                    c.advertised_fwd = c.fwd_cnt;
                    let hdr = reply(self.guest_cid, key, OP_RW, n as u32, c.fwd_cnt).encode();
                    // Straight from the connection's buffer into the guest's, header first.
                    let (front, back) = c.out.as_slices();
                    let (a, b) = if n <= front.len() { (&front[..n], &[][..]) } else { (front, &back[..n - front.len()]) };
                    let written = chain.write_parts(&self.mem, &[&hdr, a, b]).unwrap_or(0);
                    c.out.drain(..n);
                    if let Some(h) = c.handler.as_mut() {
                        h.guest_took(n);
                    }
                    // One buffer per turn: the connection goes to the back of the line.
                    if c.sendable() {
                        self.ready.push_back(key);
                    } else {
                        c.ready = false;
                    }
                    written
                }
            };
            let _ = self.rxq.add_used(&self.mem, chain.head, written);
            *used = true;
        }
        false
    }

    /// Closes the connections whose host side finished once their data has drained; true if
    /// any was (its reset is queued).
    fn reap_closing(&mut self) -> bool {
        let mut reaped = false;
        let mut i = 0;
        while let Some(&key) = self.closing.get(i) {
            match self.conns.get(&key) {
                Some(c) if c.closing && !c.out.is_empty() => i += 1,
                Some(c) if c.closing => {
                    self.closing.swap_remove(i);
                    self.close(key, true);
                    reaped = true;
                }
                _ => {
                    self.closing.swap_remove(i);
                }
            }
        }
        reaped
    }

    /// Header-only packets first, then data from the connections in turn.
    fn next_rx_work(&mut self) -> Option<RxWork> {
        if let Some(h) = self.control.pop_front() {
            return Some(RxWork::Control(h));
        }
        while let Some(key) = self.ready.pop_front() {
            match self.conns.get_mut(&key) {
                Some(c) if c.sendable() => return Some(RxWork::Data(key)),
                // Its credit shrank (the guest lowered buf_alloc) since it was queued.
                Some(c) => c.ready = false,
                None => {}
            }
        }
        None
    }

    fn unpop(&mut self, work: RxWork) {
        match work {
            RxWork::Control(h) => self.control.push_front(h),
            RxWork::Data(key) => self.ready.push_front(key),
        }
    }
}

/// A host→guest header for connection `key` (guest port, host port).
fn reply(guest_cid: u64, key: Key, op: u16, len: u32, fwd_cnt: u32) -> Hdr {
    Hdr {
        src_cid: HOST_CID,
        dst_cid: guest_cid,
        src_port: key.1,
        dst_port: key.0,
        len,
        typ: TYPE_STREAM,
        op,
        flags: 0,
        buf_alloc: BUF_ALLOC,
        fwd_cnt,
    }
}

enum RxWork {
    Control(Hdr),
    Data(Key),
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpStream};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::irq::TestLine;
    use crate::pipe::adb::{ADBD_VSOCK_PORT, AdbService};
    use crate::pipe::{HostMsg, PipeService};
    use crate::virtio::mmio::F_RING_EVENT_IDX;
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    const GUEST_CID: u64 = 77;
    const RX_BUFS: u64 = RAM_BASE + 0x10_0000;
    const BUF: u32 = 4096;

    /// Echoes guest bytes back, and remembers its contexts (the latest, and all in order) and
    /// how many pipes were closed.
    #[derive(Default)]
    struct Echo {
        ctx: Mutex<Option<PipeCtx>>,
        all: Mutex<Vec<PipeCtx>>,
        closed: Arc<Mutex<u32>>,
    }

    impl Echo {
        /// The context of the `i`th echo pipe opened, once it is.
        fn nth(&self, i: usize) -> PipeCtx {
            let start = Instant::now();
            loop {
                if let Some(c) = self.all.lock().expect("all").get(i) {
                    return c.clone();
                }
                deadline_passed(start, "the pipe never opened");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Holds every guest byte until the test lets it go, like a client that stopped reading.
    #[derive(Default)]
    struct Slow {
        held: Arc<std::sync::atomic::AtomicUsize>,
        ctx: Mutex<Option<PipeCtx>>,
        closed: Arc<Mutex<u32>>,
    }

    struct SlowPipe(Arc<std::sync::atomic::AtomicUsize>, Arc<Mutex<u32>>);

    impl PipeHandler for SlowPipe {
        fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
            self.0.fetch_add(data.len(), std::sync::atomic::Ordering::SeqCst);
            true
        }
        fn host_msg(&mut self, _msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
            true
        }
        fn backlog(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn closed(&mut self) {
            *self.1.lock().expect("closed") += 1;
        }
    }

    impl PipeService for Arc<Slow> {
        fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
            (name == "slow").then(|| {
                *self.ctx.lock().expect("ctx") = Some(ctx);
                Box::new(SlowPipe(self.held.clone(), self.closed.clone())) as Box<dyn PipeHandler>
            })
        }
    }

    impl Slow {
        /// The host side delivers `n` held bytes.
        fn release(&self, n: usize) {
            self.held.fetch_sub(n, std::sync::atomic::Ordering::SeqCst);
            assert!(self.ctx.lock().expect("ctx").as_ref().expect("opened").drained());
        }
    }

    struct EchoPipe(Arc<Mutex<u32>>);

    impl PipeHandler for EchoPipe {
        fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
            to_guest.extend_from_slice(data);
            data != b"bye"
        }
        fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
            match msg {
                // As the adb forwarder does.
                HostMsg::Data(d) if to_guest.is_empty() => {
                    *to_guest = d;
                    true
                }
                HostMsg::Data(d) => {
                    to_guest.extend(d);
                    true
                }
                _ => false,
            }
        }
        fn closed(&mut self) {
            *self.0.lock().expect("closed") += 1;
        }
    }

    impl PipeService for Arc<Echo> {
        fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
            (name == "echo").then(|| {
                self.all.lock().expect("all").push(ctx.clone());
                *self.ctx.lock().expect("ctx") = Some(ctx);
                Box::new(EchoPipe(self.closed.clone())) as Box<dyn PipeHandler>
            })
        }
    }

    /// Descriptors per queue in the test guest.
    const RING: u16 = 64;
    /// Where the guest builds tx packets: one slot per descriptor, each a header and 64 KiB.
    const TX_SLOTS: u64 = RAM_BASE + 0x20_0000;
    const SLOT: u64 = 0x11000;
    /// The receive buffer the guest advertises (Linux's default).
    const GUEST_BUF_ALLOC: u32 = 256 * 1024;

    /// What the guest knows about one connection, keyed (guest port, host port).
    #[derive(Clone, Copy, Debug, Default)]
    struct Peer {
        /// The host's latest buf_alloc and fwd_cnt.
        buf_alloc: u32,
        fwd_cnt: u32,
        /// Payload bytes sent to and received from the host.
        sent: u32,
        got: u32,
    }

    /// The guest's virtio-vsock driver, as Linux's: 4 KiB rx buffers reposted as they're used,
    /// tx packets one per chain, and credit honoured both ways. It negotiates what the device
    /// offers, kicks only when `virtqueue_kick_prepare` would, and asks for interrupts as
    /// `virtqueue_enable_cb` does once it has read the used rings.
    struct Guest {
        mem: Arc<GuestRam>,
        dev: Vsock,
        rx: Driver,
        tx: Driver,
        event_idx: bool,
        irq: Arc<Interrupt>,
        /// Kicks sent (MMIO exits on a real guest) and interrupts taken.
        kicks: u32,
        irqs: u32,
        rx_seen: u16,
        rx_posted: u16,
        /// Repost each rx buffer as soon as its packet is read (off: the test posts them).
        repost: bool,
        tx_pushed: u16,
        echo: Arc<Echo>,
        /// Packets received but not yet taken, in arrival order.
        inbox: VecDeque<(Hdr, Vec<u8>)>,
        peers: HashMap<Key, Peer>,
    }

    fn guest() -> Guest {
        guest_with(|_, _| {}, RING)
    }

    /// A guest whose registry also gets what `register` adds (given the worker's event sender),
    /// with `posted` rx buffers to start.
    fn guest_with(register: impl FnOnce(&mut PipeRegistry, &Sender<WorkerEvent>), posted: u16) -> Guest {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x100_0000).expect("ram"));
        let echo = Arc::new(Echo::default());
        let mut registry = PipeRegistry::default();
        registry.register("echo", Box::new(echo.clone()));
        let (events, rx_events) = event_channel();
        register(&mut registry, &events);
        let mut dev = Vsock::new(GUEST_CID, Arc::new(registry), events, rx_events);
        let (rx, tx) = (Driver::at(RAM_BASE + 0x1000, RING), Driver::at(RAM_BASE + 0x8000, RING));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let features = dev.features();
        let queues = vec![Some(rx.cfg), Some(tx.cfg), None];
        dev.activate(Activation { mem: mem.clone(), irq: irq.clone(), queues, features }).expect("activate");
        let mut g = Guest {
            mem,
            dev,
            rx,
            tx,
            event_idx: features & F_RING_EVENT_IDX != 0,
            irq,
            kicks: 0,
            irqs: 0,
            rx_seen: 0,
            rx_posted: 0,
            repost: true,
            tx_pushed: 0,
            echo,
            inbox: VecDeque::new(),
            peers: HashMap::new(),
        };
        g.post(posted);
        g
    }

    fn rx_buf(id: u16) -> u64 {
        RX_BUFS + u64::from(id % RING) * u64::from(BUF)
    }

    fn deadline_passed(start: Instant, what: &str) {
        assert!(start.elapsed() < Duration::from_secs(10), "{what}");
    }

    impl Guest {
        /// Posts `n` more rx buffers and tells the device. Buffers are used in order, so the
        /// descriptor the driver picks is always the one whose buffer this reuses.
        fn post(&mut self, n: u16) {
            let old = self.rx.avail_idx();
            for _ in 0..n {
                self.rx.push(&self.mem, &[(rx_buf(self.rx_posted), BUF, true)]);
                self.rx_posted = self.rx_posted.wrapping_add(1);
            }
            if n > 0 && self.rx.needs_kick(&self.mem, old, self.event_idx) {
                self.kicks += 1;
                self.dev.notify(RXQ as u16);
            }
        }

        fn send(&mut self, op: u16, src_port: u32, dst_port: u32, payload: &[u8]) {
            let start = Instant::now();
            while self.tx_pushed.wrapping_sub(self.tx.used_idx(&self.mem)) >= RING {
                deadline_passed(start, "the device stopped taking tx packets");
                std::thread::yield_now();
            }
            let key = (src_port, dst_port);
            let p = self.peers.entry(key).or_default();
            p.sent = p.sent.wrapping_add(payload.len() as u32);
            let h = Hdr {
                src_cid: GUEST_CID,
                dst_cid: HOST_CID,
                src_port,
                dst_port,
                len: payload.len() as u32,
                typ: TYPE_STREAM,
                op,
                flags: 0,
                buf_alloc: GUEST_BUF_ALLOC,
                // Everything received has been read.
                fwd_cnt: p.got,
            };
            let slot = TX_SLOTS + u64::from(self.tx_pushed % RING) * SLOT;
            self.mem.write(slot, &h.encode()).expect("tx hdr");
            self.mem.write(slot + HDR as u64, payload).expect("tx payload");
            let old = self.tx.avail_idx();
            self.tx.push(&self.mem, &[(slot, (HDR + payload.len()) as u32, false)]);
            self.tx_pushed = self.tx_pushed.wrapping_add(1);
            if self.tx.needs_kick(&self.mem, old, self.event_idx) {
                self.kicks += 1;
                self.dev.notify(TXQ as u16);
            }
        }

        /// How much the host lets the guest send on `key` now.
        fn credit(&self, key: Key) -> u32 {
            let p = self.peers.get(&key).copied().unwrap_or_default();
            p.buf_alloc.saturating_sub(p.sent.wrapping_sub(p.fwd_cnt))
        }

        /// Moves every packet the device has completed into the inbox.
        fn poll(&mut self) {
            let st = self.irq.status();
            if st != 0 {
                self.irqs += 1;
                self.irq.ack(st);
            }
            let used = self.rx.used_idx(&self.mem);
            let mut reposts = 0;
            while self.rx_seen != used {
                let (id, len) = self.rx.used(&self.mem, self.rx_seen % RING);
                self.rx_seen = self.rx_seen.wrapping_add(1);
                let mut b = vec![0u8; len as usize];
                self.mem.read(rx_buf(id as u16), &mut b).expect("rx buf");
                let h = Hdr::parse(&b).expect("header");
                let p = self.peers.entry((h.dst_port, h.src_port)).or_default();
                (p.buf_alloc, p.fwd_cnt) = (h.buf_alloc, h.fwd_cnt);
                p.got = p.got.wrapping_add(h.len);
                self.inbox.push_back((h, b[HDR..HDR + h.len as usize].to_vec()));
                reposts += 1;
            }
            self.rx.enable_cb(&self.mem, self.rx_seen, self.event_idx);
            self.tx.enable_cb(&self.mem, self.tx.used_idx(&self.mem), self.event_idx);
            if self.repost {
                self.post(reposts);
            }
        }

        /// Kicks and interrupts per packet, both ways, so far.
        fn per_packet(&self) -> String {
            let pkts = f64::from(self.rx_seen) + f64::from(self.tx_pushed);
            format!("{:.3} kicks/pkt, {:.3} irqs/pkt", f64::from(self.kicks) / pkts, f64::from(self.irqs) / pkts)
        }

        /// The next packet the device puts in an rx buffer.
        fn recv(&mut self) -> (Hdr, Vec<u8>) {
            self.recv_where(|_| true)
        }

        /// The next packet for connection `key`; others stay in the inbox.
        fn recv_on(&mut self, key: Key) -> (Hdr, Vec<u8>) {
            self.recv_where(|h| (h.dst_port, h.src_port) == key)
        }

        fn recv_where(&mut self, want: impl Fn(&Hdr) -> bool) -> (Hdr, Vec<u8>) {
            let start = Instant::now();
            loop {
                self.poll();
                if let Some(i) = self.inbox.iter().position(|(h, _)| want(h)) {
                    return self.inbox.remove(i).expect("packet");
                }
                deadline_passed(start, "no rx packet");
                std::thread::sleep(Duration::from_micros(200));
            }
        }

        fn quiet(&mut self) -> bool {
            std::thread::sleep(Duration::from_millis(50));
            self.poll();
            self.inbox.is_empty()
        }
    }

    /// Host → guest throughput through one pipe, as `adb push`/`install` stream: the guest posts
    /// 4 KiB receive buffers (Linux's default) and returns credit as it reads.
    ///     cargo test --release -p simee-emu-vmm vsock_throughput -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn vsock_throughput() {
        const TOTAL: usize = 64 << 20;
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x80_0000).expect("ram"));
        let echo = Arc::new(Echo::default());
        let mut registry = PipeRegistry::default();
        registry.register("echo", Box::new(echo.clone()));
        let (events, rx_events) = event_channel();
        let mut dev = Vsock::new(GUEST_CID, Arc::new(registry), events, rx_events);
        let size = 256u16;
        let (mut rx, mut tx) = (Driver::at(RAM_BASE + 0x1000, size), Driver::at(RAM_BASE + 0x10_000, size));
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        // Fixed rx buffers, reposted after use; the ring wraps, so the driver's descriptor ids do too.
        let buf = |i: u16| RX_BUFS + u64::from(i) * u64::from(BUF);
        for i in 0..size {
            rx.push(&mem, &[(buf(i), BUF, true)]);
        }
        let features = dev.features();
        let event_idx = features & F_RING_EVENT_IDX != 0;
        dev.activate(Activation { mem: mem.clone(), irq: irq.clone(), queues: vec![Some(rx.cfg), Some(tx.cfg), None], features })
            .expect("activate");
        let mut tx_addr = RAM_BASE + 0x40_0000;
        // Kicks only when the device asked for them (Linux's `virtqueue_kick_prepare`); counted.
        let mut kicks = 0u32;
        let mut send = |dev: &mut Vsock, tx: &mut Driver, op: u16, fwd: u32, payload: &[u8]| {
            let h = Hdr {
                src_cid: GUEST_CID,
                dst_cid: HOST_CID,
                src_port: 1,
                dst_port: PIPE_PORT,
                len: payload.len() as u32,
                typ: TYPE_STREAM,
                op,
                flags: 0,
                buf_alloc: 256 * 1024,
                fwd_cnt: fwd,
            };
            let mut pkt = h.encode().to_vec();
            pkt.extend_from_slice(payload);
            mem.write(tx_addr, &pkt).expect("tx");
            let old = tx.avail_idx();
            tx.push(&mem, &[(tx_addr, pkt.len() as u32, false)]);
            tx_addr = if tx_addr > RAM_BASE + 0x70_0000 { RAM_BASE + 0x40_0000 } else { tx_addr + 0x100 };
            if tx.needs_kick(&mem, old, event_idx) {
                kicks += 1;
                dev.notify(TXQ as u16);
            }
        };
        send(&mut dev, &mut tx, OP_REQUEST, 0, &[]);
        send(&mut dev, &mut tx, OP_RW, 0, b"pipe:echo\0");
        let ctx = loop {
            if let Some(c) = echo.ctx.lock().expect("ctx").clone() {
                break c;
            }
            std::thread::yield_now();
        };
        let chunk = vec![0x5au8; 64 * 1024];
        let start = Instant::now();
        let producer = std::thread::spawn(move || {
            for _ in 0..TOTAL / chunk.len() {
                ctx.send(HostMsg::Data(chunk.clone()));
            }
        });
        let (mut seen, mut got, mut credited) = (0u16, 0usize, 0usize);
        let (mut rx_pkts, mut rx_kicks, mut irqs) = (0u32, 0u32, 0u32);
        while got < TOTAL {
            let st = irq.status();
            if st != 0 {
                irqs += 1;
                irq.ack(st);
            }
            let used = rx.used_idx(&mem);
            if used == seen {
                std::thread::yield_now();
                continue;
            }
            let old = rx.avail_idx();
            while seen != used {
                rx_pkts += 1;
                let (id, len) = rx.used(&mem, seen % size);
                seen = seen.wrapping_add(1);
                let h = Hdr::parse(&{
                    let mut b = [0u8; HDR];
                    mem.read(buf(id as u16 % size), &mut b).expect("hdr");
                    b
                })
                .expect("hdr");
                if h.op == OP_RW {
                    got += len as usize - HDR;
                }
                rx.push(&mem, &[(buf(id as u16 % size), BUF, true)]);
            }
            rx.enable_cb(&mem, seen, event_idx);
            tx.enable_cb(&mem, tx.used_idx(&mem), event_idx);
            if rx.needs_kick(&mem, old, event_idx) {
                rx_kicks += 1;
                dev.notify(RXQ as u16);
            }
            if got - credited >= 64 * 1024 {
                send(&mut dev, &mut tx, OP_CREDIT_UPDATE, got as u32, &[]);
                credited = got;
            }
        }
        let secs = start.elapsed().as_secs_f64();
        producer.join().expect("producer");
        let pkts = f64::from(rx_pkts);
        println!(
            "vsock host->guest: {} MiB in {:.3} s = {:.0} MiB/s, {:.3} kicks/pkt, {:.3} irqs/pkt ({rx_pkts} rx packets)",
            TOTAL >> 20,
            secs,
            (TOTAL >> 20) as f64 / secs,
            f64::from(kicks + rx_kicks) / pkts,
            f64::from(irqs) / pkts
        );
    }

    /// Guest → host throughput through one pipe, as `adb pull`/`exec-out` stream: the guest
    /// sends 64 KiB packets (Linux's largest) into a sink.
    ///     cargo test --release -p simee-emu-vmm vsock_throughput_to_host -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn vsock_throughput_to_host() {
        struct Count(Arc<std::sync::atomic::AtomicUsize>);
        impl PipeHandler for Count {
            fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
                self.0.fetch_add(data.len(), std::sync::atomic::Ordering::Relaxed);
                true
            }
            fn host_msg(&mut self, _msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
                true
            }
        }
        struct Sink(Arc<std::sync::atomic::AtomicUsize>);
        impl PipeService for Sink {
            fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
                (name == "sink").then(|| Box::new(Count(self.0.clone())) as Box<dyn PipeHandler>)
            }
        }
        const TOTAL: usize = 256 << 20;
        const PKT: usize = 64 * 1024;
        let got = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x200_0000).expect("ram"));
        let mut registry = PipeRegistry::default();
        registry.register("sink", Box::new(Sink(got.clone())));
        let (events, rx_events) = event_channel();
        let mut dev = Vsock::new(GUEST_CID, Arc::new(registry), events, rx_events);
        let size = 64u16;
        let (mut rx, mut tx) = (Driver::at(RAM_BASE + 0x1000, size), Driver::at(RAM_BASE + 0x8000, size));
        for i in 0..size {
            rx.push(&mem, &[(RX_BUFS + u64::from(i) * u64::from(BUF), BUF, true)]);
        }
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let features = dev.features();
        let event_idx = features & F_RING_EVENT_IDX != 0;
        dev.activate(Activation { mem: mem.clone(), irq: irq.clone(), queues: vec![Some(rx.cfg), Some(tx.cfg), None], features })
            .expect("activate");
        // Packets prebuilt in guest RAM, one slot per descriptor, reused as the ring wraps.
        let slot = |i: u16| RAM_BASE + 0x40_0000 + u64::from(i) * (HDR + PKT) as u64;
        let packet = |payload: &[u8], fwd: u32| {
            let h = Hdr {
                src_cid: GUEST_CID,
                dst_cid: HOST_CID,
                src_port: 1,
                dst_port: PIPE_PORT,
                len: payload.len() as u32,
                typ: TYPE_STREAM,
                op: OP_RW,
                flags: 0,
                buf_alloc: 256 * 1024,
                fwd_cnt: fwd,
            };
            let mut p = h.encode().to_vec();
            p.extend_from_slice(payload);
            p
        };
        let mut req = packet(&[], 0);
        req[30] = OP_REQUEST as u8;
        mem.write(slot(0), &req).expect("req");
        tx.push(&mem, &[(slot(0), HDR as u32, false)]);
        let name = packet(b"pipe:sink\0", 0);
        mem.write(slot(1), &name).expect("name");
        tx.push(&mem, &[(slot(1), name.len() as u32, false)]);
        dev.notify(TXQ as u16);
        // The request and name must be read before their slots are reused.
        let deadline = Instant::now() + Duration::from_secs(5);
        while tx.used_idx(&mem) < 2 {
            assert!(Instant::now() < deadline, "the connection never opened");
            std::thread::yield_now();
        }
        let data = packet(&vec![0xa5u8; PKT], 0);
        for i in 0..size {
            mem.write(slot(i), &data).expect("slot");
        }
        let start = Instant::now();
        let mut sent = 0usize;
        let mut pushed = 2u16;
        // As Linux's driver: send only within the host's credit, learnt from the fwd_cnt of
        // whatever it sends back (credit updates), reposting those rx buffers.
        let (mut seen, mut host_fwd) = (0u16, 0u32);
        let (mut kicks, mut irqs) = (0u32, 0u32);
        let named = name.len() - HDR;
        while sent < TOTAL {
            let st = irq.status();
            if st != 0 {
                irqs += 1;
                irq.ack(st);
            }
            let used = rx.used_idx(&mem);
            let old = rx.avail_idx();
            while seen != used {
                let (id, _) = rx.used(&mem, seen % size);
                seen = seen.wrapping_add(1);
                let at = RX_BUFS + u64::from(id) * u64::from(BUF);
                let mut b = [0u8; HDR];
                mem.read(at, &mut b).expect("hdr");
                host_fwd = Hdr::parse(&b).expect("hdr").fwd_cnt;
                rx.push(&mem, &[(at, BUF, true)]);
            }
            rx.enable_cb(&mem, seen, event_idx);
            tx.enable_cb(&mem, tx.used_idx(&mem), event_idx);
            if rx.avail_idx() != old && rx.needs_kick(&mem, old, event_idx) {
                kicks += 1;
                dev.notify(RXQ as u16);
            }
            let in_flight = |sent: usize| (named + sent) as u32 - host_fwd;
            // Keep the ring as full as the queue and the credit allow.
            let old = tx.avail_idx();
            while pushed.wrapping_sub(tx.used_idx(&mem)) < size && sent < TOTAL && in_flight(sent) as usize + PKT <= BUF_ALLOC as usize {
                tx.push(&mem, &[(slot(pushed % size), (HDR + PKT) as u32, false)]);
                pushed = pushed.wrapping_add(1);
                sent += PKT;
            }
            if tx.avail_idx() != old && tx.needs_kick(&mem, old, event_idx) {
                kicks += 1;
                dev.notify(TXQ as u16);
            }
            std::thread::yield_now();
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while got.load(std::sync::atomic::Ordering::Relaxed) < TOTAL {
            assert!(Instant::now() < deadline, "only {} bytes arrived", got.load(std::sync::atomic::Ordering::Relaxed));
            std::thread::yield_now();
        }
        let secs = start.elapsed().as_secs_f64();
        let pkts = f64::from(pushed);
        println!(
            "vsock guest->host: {} MiB in {:.3} s = {:.0} MiB/s, {:.3} kicks/pkt, {:.3} irqs/pkt ({pushed} tx packets)",
            TOTAL >> 20,
            secs,
            (TOTAL >> 20) as f64 / secs,
            f64::from(kicks) / pkts,
            f64::from(irqs) / pkts
        );
    }

    /// An adb client on the bridge's port whose vsock connection the guest's adbd accepted.
    fn adb_guest() -> (Guest, TcpStream, Key) {
        let mut port = 0;
        let mut g = guest_with(
            |registry, events| {
                let svc = AdbService::bind(0, events.clone()).expect("bind");
                port = svc.port;
                registry.register("qemud:adb", Box::new(svc));
            },
            RING,
        );
        let client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        let (h, _) = g.recv();
        assert_eq!((h.op, h.dst_port), (OP_REQUEST, ADBD_VSOCK_PORT));
        let key = (h.dst_port, h.src_port);
        g.send(OP_RESPONSE, key.0, key.1, &[]);
        (g, client, key)
    }

    /// `adb push` end to end: a host client writes into the bridge, the guest reads 4 KiB
    /// buffers and returns credit every 64 KiB.
    ///     cargo test --release -p simee-emu-vmm adb_throughput -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn adb_throughput_to_guest() {
        const TOTAL: usize = 64 << 20;
        let (mut g, mut client, key) = adb_guest();
        let writer = std::thread::spawn(move || {
            let chunk = vec![0x5au8; 64 * 1024];
            for _ in 0..TOTAL / chunk.len() {
                client.write_all(&chunk).expect("write");
            }
            client
        });
        let start = Instant::now();
        let mut credited = 0u32;
        loop {
            g.poll();
            g.inbox.clear();
            let got = g.peers[&key].got;
            if got as usize >= TOTAL {
                break;
            }
            if got - credited >= 64 * 1024 {
                g.send(OP_CREDIT_UPDATE, key.0, key.1, &[]);
                credited = got;
            }
            std::thread::yield_now();
        }
        let secs = start.elapsed().as_secs_f64();
        drop(writer.join().expect("writer"));
        println!("adb host->guest: {} MiB in {:.3} s = {:.0} MiB/s, {}", TOTAL >> 20, secs, (TOTAL >> 20) as f64 / secs, g.per_packet());
    }

    /// `adb pull` end to end: the guest sends 64 KiB packets as its credit allows, a host client
    /// reads them off the bridge.
    ///     cargo test --release -p simee-emu-vmm adb_throughput -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn adb_throughput_to_host() {
        const TOTAL: usize = 256 << 20;
        let (mut g, mut client, key) = adb_guest();
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0u8; 256 * 1024];
            let mut got = 0;
            while got < TOTAL {
                let n = client.read(&mut buf).expect("read");
                assert!(n > 0, "the bridge hung up after {got} bytes");
                got += n;
            }
        });
        let payload = vec![0xa5u8; 64 * 1024];
        let start = Instant::now();
        let mut sent = 0;
        while sent < TOTAL {
            g.poll();
            g.inbox.clear();
            let n = (g.credit(key) as usize).min(payload.len()).min(TOTAL - sent);
            if n == 0 {
                std::thread::yield_now();
                continue;
            }
            g.send(OP_RW, key.0, key.1, &payload[..n]);
            sent += n;
        }
        reader.join().expect("reader");
        let secs = start.elapsed().as_secs_f64();
        println!("adb guest->host: {} MiB in {:.3} s = {:.0} MiB/s, {}", TOTAL >> 20, secs, (TOTAL >> 20) as f64 / secs, g.per_packet());
    }

    /// Request/response through one pipe, as a qemud service or `adb shell` round trip: the guest
    /// sends 64 bytes and waits (spinning, as a busy guest would be in its interrupt handler) for
    /// the echo. Kicks and interrupts per packet are what each round costs vCPU0.
    ///     cargo test --release -p simee-emu-vmm vsock_ping_pong -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn vsock_ping_pong() {
        const ROUNDS: u32 = 20_000;
        let mut g = guest();
        let key = (11, PIPE_PORT);
        g.send(OP_REQUEST, key.0, key.1, &[]);
        assert_eq!(g.recv_on(key).0.op, OP_RESPONSE);
        g.send(OP_RW, key.0, key.1, b"pipe:echo\0");
        g.echo.nth(0);
        let (k0, i0, rx0, tx0) = (g.kicks, g.irqs, g.rx_seen, g.tx_pushed);
        let msg = [0x42u8; 64];
        let start = Instant::now();
        for _ in 0..ROUNDS {
            g.send(OP_RW, key.0, key.1, &msg);
            loop {
                g.poll();
                if let Some((h, d)) = g.inbox.pop_front() {
                    assert_eq!((h.op, d.len()), (OP_RW, msg.len()));
                    break;
                }
                std::hint::spin_loop();
            }
        }
        let secs = start.elapsed().as_secs_f64();
        let pkts = f64::from(g.rx_seen.wrapping_sub(rx0)) + f64::from(g.tx_pushed.wrapping_sub(tx0));
        println!(
            "vsock ping-pong 64 B: {:.1} us/round, {:.3} kicks/pkt, {:.3} irqs/pkt",
            secs * 1e6 / f64::from(ROUNDS),
            f64::from(g.kicks - k0) / pkts,
            f64::from(g.irqs - i0) / pkts
        );
    }

    /// The guest sends on `key` as fast as its credit allows until the host has withheld credit
    /// for 200 ms; returns the bytes sent.
    fn send_until_stalled(g: &mut Guest, key: Key) -> usize {
        let chunk = vec![0x11u8; 64 * 1024];
        let (mut sent, mut last) = (0usize, Instant::now());
        while last.elapsed() < Duration::from_millis(200) {
            g.poll();
            g.inbox.clear();
            let n = (g.credit(key) as usize).min(chunk.len());
            if n == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            assert!(sent < 256 << 20, "the host never withheld credit");
            g.send(OP_RW, key.0, key.1, &chunk[..n]);
            sent += n;
            last = Instant::now();
        }
        sent
    }

    // Unix only: it binds the adb bridge, which answers `Unsupported` on Windows.
    #[cfg(unix)]
    #[test]
    fn a_stalled_adb_client_holds_up_neither_other_pipes_nor_other_sessions() {
        let (mut g, mut stalled, adb) = adb_guest();
        // `adb logcat` into a client that stopped reading (kill -STOP): once its socket is full,
        // the guest runs out of credit instead of the worker blocking on the socket.
        let sent = send_until_stalled(&mut g, adb);
        assert_eq!(g.credit(adb), 0, "credit is withheld");
        let held = sent - g.peers[&adb].fwd_cnt as usize;
        assert!(held <= BUF_ALLOC as usize, "the host holds at most its advertised buffer ({held} bytes)");

        // Another pipe still answers at once.
        let start = Instant::now();
        g.send(OP_REQUEST, 1024, PIPE_PORT, &[]);
        assert_eq!(g.recv_on((1024, PIPE_PORT)).0.op, OP_RESPONSE);
        g.send(OP_RW, 1024, PIPE_PORT, b"pipe:echo\0ping");
        assert_eq!(g.recv_on((1024, PIPE_PORT)).1, b"ping");
        // And so does another adb session, both ways.
        let port = stalled.peer_addr().expect("addr").port();
        let mut other = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        let (h, _) = g.recv_where(|h| h.op == OP_REQUEST);
        let second = (h.dst_port, h.src_port);
        g.send(OP_RESPONSE, second.0, second.1, &[]);
        other.write_all(b"CNXN").expect("write");
        assert_eq!(g.recv_on(second).1, b"CNXN");
        g.send(OP_RW, second.0, second.1, b"AUTH");
        other.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let mut b = [0u8; 4];
        other.read_exact(&mut b).expect("read");
        assert_eq!(&b, b"AUTH");
        assert!(start.elapsed() < Duration::from_secs(1), "other traffic waited {:?}", start.elapsed());

        // The client resumes: it gets every byte, and the guest its credit back unasked.
        stalled.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let mut all = vec![0u8; sent];
        stalled.read_exact(&mut all).expect("everything the guest sent");
        assert!(all.iter().all(|&b| b == 0x11));
        let start = Instant::now();
        while g.peers[&adb].fwd_cnt as usize != sent {
            deadline_passed(start, "the freed credit was never returned");
            g.poll();
            g.inbox.clear();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(g.credit(adb), BUF_ALLOC);
    }

    /// `network loss` is the NAT's: adb's own channel (vsock, outside the NAT) loses nothing,
    /// even while the NAT drops every packet both ways.
    #[cfg(unix)]
    #[test]
    fn adb_loses_nothing_while_the_nat_drops_every_packet() {
        use crate::virtio::net::Backend;
        let network =
            Arc::new(Mutex::new(sim_core::network::Conditions { upload_loss_pct: 100.0, download_loss_pct: 100.0, ..Default::default() }));
        let mut nat = crate::net::Nat::new(crate::net::Config { network, loss_seed: Some(1), ..Default::default() }).expect("nat");
        nat.attach(crate::virtio::net::RxReady::new(Arc::new(crate::virtio::Doorbell::default())));
        let (mut g, mut client, adb) = adb_guest();
        client.write_all(b"CNXN").expect("write");
        assert_eq!(g.recv_on(adb).1, b"CNXN", "host to guest");
        g.send(OP_RW, adb.0, adb.1, b"AUTH");
        client.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let mut b = [0u8; 4];
        client.read_exact(&mut b).expect("guest to host");
        assert_eq!(&b, b"AUTH");
        drop(nat);
    }

    #[test]
    fn a_guest_pipe_connects_names_its_service_and_echoes() {
        let mut g = guest();
        let mut cfg = [0u8; 8];
        g.dev.read_config(0, &mut cfg);
        assert_eq!(u64::from_le_bytes(cfg), GUEST_CID);

        g.send(OP_REQUEST, 1024, PIPE_PORT, &[]);
        let (h, _) = g.recv();
        assert_eq!((h.op, h.src_cid, h.dst_cid, h.src_port, h.dst_port), (OP_RESPONSE, HOST_CID, GUEST_CID, PIPE_PORT, 1024));
        // The name may arrive split, and data may follow it in the same packet.
        g.send(OP_RW, 1024, PIPE_PORT, b"pipe:ec");
        g.send(OP_RW, 1024, PIPE_PORT, b"ho\0hello");
        let (h, data) = g.recv();
        assert_eq!((h.op, data.as_slice()), (OP_RW, &b"hello"[..]));
        assert_eq!(h.fwd_cnt, 15, "everything the guest sent counts as forwarded");

        // The host side talks through the context the service got.
        let ctx = g.echo.ctx.lock().expect("ctx").clone().expect("opened");
        assert!(ctx.send(HostMsg::Data(b"from host".to_vec())));
        let (_, data) = g.recv();
        assert_eq!(data, b"from host");

        // The handler ending the conversation resets the connection and closes the pipe.
        g.send(OP_RW, 1024, PIPE_PORT, b"bye");
        let (h, data) = g.recv();
        assert_eq!((h.op, data.as_slice()), (OP_RST, &[][..]));
        assert_eq!(*g.echo.closed.lock().expect("closed"), 1);
        g.send(OP_RW, 1024, PIPE_PORT, b"late");
        assert_eq!(g.recv().0.op, OP_RST, "a closed connection is reset");
    }

    #[test]
    fn unknown_pipes_ports_and_peers_are_reset() {
        let mut g = guest();
        g.send(OP_REQUEST, 1, 1234, &[]);
        assert_eq!(g.recv().0.op, OP_RST, "only the pipe port listens");
        g.send(OP_REQUEST, 2, PIPE_PORT, &[]);
        assert_eq!(g.recv().0.op, OP_RESPONSE);
        g.send(OP_REQUEST, 2, PIPE_PORT, &[]);
        assert_eq!(g.recv().0.op, OP_RST, "a second request on a live connection");
        g.send(OP_RW, 3, PIPE_PORT, b"pipe:nope\0");
        assert_eq!(g.recv().0.op, OP_RST, "data on no connection");
        g.send(OP_REQUEST, 4, PIPE_PORT, &[]);
        assert_eq!(g.recv().0.op, OP_RESPONSE);
        g.send(OP_RW, 4, PIPE_PORT, b"pipe:nope\0");
        assert_eq!(g.recv().0.op, OP_RST, "no such service");
        g.send(OP_RST, 9, PIPE_PORT, &[]);
        assert!(g.quiet(), "a reset is never answered");
        g.send(OP_SHUTDOWN, 2, PIPE_PORT, &[]);
        assert_eq!(g.recv().0.op, OP_RST, "shutdown closes");
    }

    #[test]
    fn credit_requests_are_answered() {
        let mut g = guest();
        g.send(OP_REQUEST, 5, PIPE_PORT, &[]);
        g.recv();
        g.send(OP_RW, 5, PIPE_PORT, b"pipe:echo\0");
        g.send(OP_CREDIT_REQUEST, 5, PIPE_PORT, &[]);
        let (h, _) = g.recv();
        assert_eq!((h.op, h.fwd_cnt, h.buf_alloc), (OP_CREDIT_UPDATE, 10, BUF_ALLOC));
    }

    #[test]
    fn host_connections_start_once_the_guest_answers() {
        let mut g = guest();
        let made = Arc::new(Mutex::new(Vec::new()));
        let refused = Arc::new(Mutex::new(0));
        let connect = |g: &Guest, port: u32| {
            let (made, refused) = (made.clone(), refused.clone());
            let make: crate::pipe::MakeHandler = Box::new(move |ctx: PipeCtx| {
                made.lock().expect("made").push(ctx.conn);
                Box::new(EchoPipe(Arc::default())) as Box<dyn PipeHandler>
            });
            let refused: Box<dyn FnOnce() + Send> = Box::new(move || *refused.lock().expect("refused") += 1);
            g.dev.events.send(WorkerEvent::Connect { guest_port: port, make, refused }).expect("connect");
        };
        connect(&g, 5555);
        let (h, _) = g.recv();
        assert_eq!((h.op, h.dst_port, h.src_port), (OP_REQUEST, 5555, 50_000));
        assert!(made.lock().expect("made").is_empty(), "no host I/O before the guest accepts");
        g.send(OP_RESPONSE, 5555, 50_000, &[]);
        g.send(OP_RW, 5555, 50_000, b"ping");
        assert_eq!(g.recv().1, b"ping");
        assert_eq!(made.lock().expect("made").len(), 1);

        connect(&g, 6000);
        let (h, _) = g.recv();
        g.send(OP_RST, 6000, h.src_port, &[]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while *refused.lock().expect("refused") == 0 {
            assert!(Instant::now() < deadline, "refusal not reported");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(g.quiet(), "a refused request isn't reset back");
    }

    /// A guest with a `slow` pipe open on (7, PIPE_PORT).
    fn slow_guest() -> (Guest, Arc<Slow>, Key) {
        let slow = Arc::new(Slow::default());
        let s = slow.clone();
        let mut g = guest_with(move |registry, _| registry.register("slow", Box::new(s)), RING);
        let key = (7, PIPE_PORT);
        g.send(OP_REQUEST, key.0, key.1, &[]);
        assert_eq!(g.recv_on(key).0.op, OP_RESPONSE);
        g.send(OP_RW, key.0, key.1, b"pipe:slow\0");
        (g, slow, key)
    }

    #[test]
    fn credit_is_withheld_while_a_pipe_holds_bytes_and_returned_unasked() {
        let (mut g, slow, key) = slow_guest();
        let sent = send_until_stalled(&mut g, key);
        // The name was taken at once; everything after it is held.
        assert_eq!(sent, BUF_ALLOC as usize - 10, "the guest may fill the advertised buffer");
        assert_eq!(slow.held.load(std::sync::atomic::Ordering::SeqCst), sent);
        assert_eq!(g.credit(key), 0);

        // A little freed: not worth a packet while the guest still has room to wait for more.
        slow.release(64 * 1024);
        assert!(g.quiet(), "no update for 64 KiB");
        // Half the buffer freed: an update the guest never asked for.
        slow.release(64 * 1024);
        let (h, _) = g.recv_on(key);
        assert_eq!((h.op, h.fwd_cnt), (OP_CREDIT_UPDATE, 10 + 128 * 1024));
        // The backlog gone: the rest at once, whatever its size.
        slow.release(sent - 128 * 1024);
        let (h, _) = g.recv_on(key);
        assert_eq!((h.op, h.fwd_cnt), (OP_CREDIT_UPDATE, 10 + sent as u32));
        assert_eq!(g.credit(key), BUF_ALLOC);
        assert!(g.quiet());
    }

    #[test]
    fn a_guest_that_sends_past_its_credit_is_reset() {
        let (mut g, slow, key) = slow_guest();
        send_until_stalled(&mut g, key);
        g.send(OP_RW, key.0, key.1, b"one more");
        assert_eq!(g.recv_on(key).0.op, OP_RST);
        assert_eq!(*slow.closed.lock().expect("closed"), 1);
    }

    #[test]
    fn connections_take_turns_for_rx_buffers() {
        let mut g = guest_with(|_, _| {}, 0);
        g.repost = false;
        let keys = [(1, PIPE_PORT), (2, PIPE_PORT), (3, PIPE_PORT)];
        for (i, k) in keys.iter().enumerate() {
            g.post(1);
            g.send(OP_REQUEST, k.0, k.1, &[]);
            assert_eq!(g.recv_on(*k).0.op, OP_RESPONSE);
            g.send(OP_RW, k.0, k.1, b"pipe:echo\0");
            g.echo.nth(i);
        }
        // Each queues more than a buffer's worth while the guest has no buffers posted.
        for (i, fill) in b"abc".iter().copied().enumerate() {
            assert!(g.echo.nth(i).send(HostMsg::Data(vec![fill; 16 * 1024])));
        }
        std::thread::sleep(Duration::from_millis(50));
        g.post(9);
        let order: Vec<u8> = (0..9).map(|_| g.recv().1[0]).collect();
        assert_eq!(order, b"abcabcabc", "a buffer each in turn, not one connection's queue at a time");
    }

    #[test]
    fn replies_wait_for_rx_buffers_without_piling_up() {
        let mut g = guest_with(|_, _| {}, 0);
        g.repost = false;
        // Packets for no connection, each answered by a reset, while no rx buffers are posted:
        // the device takes a queue's worth, then leaves the rest in the tx ring.
        for port in 0..u32::from(RING) + 8 {
            g.send(OP_RW, 100 + port, PIPE_PORT, b"x");
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(g.tx.used_idx(&g.mem), RING, "replies held back tx");
        g.post(RING);
        for port in 0..u32::from(RING) {
            assert_eq!(g.recv_on((100 + port, PIPE_PORT)).0.op, OP_RST);
        }
        // With room again, the waiting packets are taken without another kick.
        g.post(8);
        for port in u32::from(RING)..u32::from(RING) + 8 {
            assert_eq!(g.recv_on((100 + port, PIPE_PORT)).0.op, OP_RST);
        }
        assert_eq!(g.tx.used_idx(&g.mem), RING + 8);
    }

    #[test]
    fn a_closing_pipe_drains_its_data_before_the_reset() {
        let mut g = guest();
        let key = (9, PIPE_PORT);
        g.send(OP_REQUEST, key.0, key.1, &[]);
        assert_eq!(g.recv_on(key).0.op, OP_RESPONSE);
        g.send(OP_RW, key.0, key.1, b"pipe:echo\0");
        let ctx = g.echo.nth(0);
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        assert!(ctx.send(HostMsg::Data(data.clone())));
        assert!(ctx.send(HostMsg::Close));
        let mut got = Vec::new();
        loop {
            let (h, d) = g.recv_on(key);
            if h.op == OP_RST {
                break;
            }
            assert_eq!(h.op, OP_RW);
            got.extend(d);
        }
        assert_eq!(got, data, "everything queued arrives before the reset");
        assert_eq!(*g.echo.closed.lock().expect("closed"), 1);
        g.send(OP_RW, key.0, key.1, b"late");
        assert_eq!(g.recv_on(key).0.op, OP_RST);
    }

    /// A pipe whose first guest write blocks the worker (mid-drain) until the test lets it go.
    struct Gate {
        entered: std::sync::mpsc::Sender<()>,
        release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
    }

    impl PipeService for Gate {
        fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
            type Release = Arc<Mutex<std::sync::mpsc::Receiver<()>>>;
            struct Held(Option<(std::sync::mpsc::Sender<()>, Release)>);
            impl PipeHandler for Held {
                fn guest_data(&mut self, _data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
                    if let Some((entered, release)) = self.0.take() {
                        let _ = entered.send(());
                        let _ = release.lock().expect("release").recv();
                    }
                    true
                }
                fn host_msg(&mut self, _msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
                    true
                }
            }
            (name == "gate").then(|| Box::new(Held(Some((self.entered.clone(), self.release.clone())))) as Box<dyn PipeHandler>)
        }
    }

    /// Packets the guest sends while the worker drains the tx ring cost it no kicks (event-idx:
    /// the worker stopped asking for them while it drains). Before, every packet kicked.
    #[test]
    fn a_burst_sent_while_the_worker_drains_costs_no_kicks() {
        const N: u32 = 32;
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let gate = Gate { entered: entered_tx, release: Arc::new(Mutex::new(release_rx)) };
        let mut g = guest_with(move |registry, _| registry.register("gate", Box::new(gate)), RING);
        // Dropped before the guest, so a failing assertion releases the worker instead of
        // leaving the device's drop waiting on it.
        let release = release;
        assert!(g.event_idx, "the device offers event-idx");
        let key = (21, PIPE_PORT);
        g.send(OP_REQUEST, key.0, key.1, &[]);
        assert_eq!(g.recv_on(key).0.op, OP_RESPONSE);
        g.send(OP_RW, key.0, key.1, b"pipe:gate\0");
        g.send(OP_RW, key.0, key.1, b"first");
        entered.recv_timeout(Duration::from_secs(5)).expect("the worker took the first packet");
        let kicks = g.kicks;
        for _ in 0..N {
            g.send(OP_RW, key.0, key.1, b"more");
        }
        assert_eq!(g.kicks - kicks, 0, "{N} packets sent mid-drain cost no kick");
        release.send(()).expect("release");
        let start = Instant::now();
        while g.tx.used_idx(&g.mem) != g.tx_pushed {
            deadline_passed(start, "the worker never took the burst");
            std::thread::yield_now();
        }
        // Caught up: the next packet is taken too (kicked, or found by the worker's recheck).
        g.send(OP_RW, key.0, key.1, b"after");
        while g.tx.used_idx(&g.mem) != g.tx_pushed {
            deadline_passed(start, "the packet after the burst was never taken");
            std::thread::yield_now();
        }
    }

    /// The lost-wakeup window on the tx queue: the guest publishes after the worker's last pop
    /// came back empty but before kicks are back on, so it doesn't kick. The worker still takes
    /// the packet (its recheck after re-enabling), in both suppression modes.
    #[test]
    fn a_packet_sent_as_tx_kicks_come_back_on_is_still_taken() {
        for event_idx in [true, false] {
            let mem = Arc::new(GuestRam::new(RAM_BASE, 0x100_0000).expect("ram"));
            let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
            let (events, _keep) = event_channel();
            let (rx, tx) = (Driver::at(RAM_BASE + 0x1000, RING), Arc::new(Mutex::new(Driver::at(RAM_BASE + 0x8000, RING))));
            let txcfg = tx.lock().expect("tx").cfg;
            let mut w = Worker::new(
                mem.clone(),
                irq,
                Queue::new(rx.cfg, event_idx),
                Queue::new(txcfg, event_idx),
                GUEST_CID,
                Arc::new(PipeRegistry::default()),
                events,
            );
            let request = move |mem: &GuestRam, tx: &mut Driver, port: u32| {
                let slot = TX_SLOTS + u64::from(tx.avail_idx() % RING) * SLOT;
                let h = Hdr {
                    src_cid: GUEST_CID,
                    dst_cid: HOST_CID,
                    src_port: port,
                    dst_port: PIPE_PORT,
                    typ: TYPE_STREAM,
                    op: OP_REQUEST,
                    ..Hdr::default()
                };
                mem.write(slot, &h.encode()).expect("hdr");
                let old = tx.avail_idx();
                tx.push(mem, &[(slot, HDR as u32, false)]);
                tx.needs_kick(mem, old, event_idx)
            };
            request(&mem, &mut tx.lock().expect("tx"), 1);
            let missed = Arc::new(Mutex::new(Vec::new()));
            let (m, t, missed2) = (mem.clone(), tx.clone(), missed.clone());
            let mut fired = false;
            w.txq.on_empty = Some(Box::new(move || {
                if !std::mem::replace(&mut fired, true) {
                    let kicked = request(&m, &mut t.lock().expect("tx"), 2);
                    missed2.lock().expect("missed").push(!kicked);
                }
            }));
            w.process_tx();
            assert_eq!(*missed.lock().expect("missed"), [true], "event_idx={event_idx}: the guest skipped its kick");
            assert_eq!(tx.lock().expect("tx").used_idx(&mem), 2, "event_idx={event_idx}: both packets taken");
            let answered: Vec<(u16, u32)> = w.control.iter().map(|h| (h.op, h.dst_port)).collect();
            assert_eq!(answered, [(OP_RESPONSE, 1), (OP_RESPONSE, 2)]);
        }
    }

    /// Rx buffers: a worker with replies and no buffers asks for the refill (the guest's next
    /// post kicks, and the reply goes out on it); one with buffers to spare asks for none, so
    /// the refill Linux does after every rx interrupt costs no exit. Both suppression modes.
    #[test]
    fn rx_refills_kick_only_a_starved_worker() {
        for event_idx in [true, false] {
            let mem = Arc::new(GuestRam::new(RAM_BASE, 0x100_0000).expect("ram"));
            let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
            let (events, _keep) = event_channel();
            let (mut rx, tx) = (Driver::at(RAM_BASE + 0x1000, RING), Driver::at(RAM_BASE + 0x8000, RING));
            let mut w = Worker::new(
                mem.clone(),
                irq,
                Queue::new(rx.cfg, event_idx),
                Queue::new(tx.cfg, event_idx),
                GUEST_CID,
                Arc::new(PipeRegistry::default()),
                events,
            );
            let post = |rx: &mut Driver, n: u16| {
                let old = rx.avail_idx();
                for _ in 0..n {
                    let id = rx.avail_idx();
                    rx.push(&mem, &[(rx_buf(id), BUF, true)]);
                }
                rx.needs_kick(&mem, old, event_idx)
            };
            assert!(post(&mut rx, 2), "the first buffers kick");
            for port in 0..3 {
                w.control.push_back(reply(GUEST_CID, (port, PIPE_PORT), OP_RST, 0, 0));
            }
            w.flush();
            assert_eq!((rx.used_idx(&mem), w.control.len()), (2, 1), "event_idx={event_idx}: starved with a reply left");
            assert!(post(&mut rx, 4), "event_idx={event_idx}: a starved worker asks for the refill");
            w.flush();
            assert_eq!((rx.used_idx(&mem), w.control.len()), (3, 0));
            assert!(!post(&mut rx, 1), "event_idx={event_idx}: buffers to spare, so no kick");
        }
    }

    /// The guest streams small packets at an echo pipe while the worker drains concurrently, both
    /// kicking only when the other asked: every byte comes back (no lost wakeup either way), for
    /// far fewer kicks than packets.
    #[test]
    fn a_racing_guest_never_loses_a_wakeup() {
        const PACKETS: u32 = 3000;
        let mut g = guest();
        let key = (31, PIPE_PORT);
        g.send(OP_REQUEST, key.0, key.1, &[]);
        assert_eq!(g.recv_on(key).0.op, OP_RESPONSE);
        g.send(OP_RW, key.0, key.1, b"pipe:echo\0");
        let kicks = g.kicks;
        let mut back = 0usize;
        for i in 0..PACKETS {
            g.send(OP_RW, key.0, key.1, &[i as u8; 8]);
            if i % 7 == 0 {
                g.poll();
                back += g.inbox.drain(..).map(|(_, d)| d.len()).sum::<usize>();
            }
        }
        let start = Instant::now();
        while back < PACKETS as usize * 8 {
            deadline_passed(start, "lost wakeup: echoes stopped");
            g.poll();
            back += g.inbox.drain(..).map(|(_, d)| d.len()).sum::<usize>();
            std::thread::yield_now();
        }
        assert_eq!(back, PACKETS as usize * 8);
        assert!(g.kicks - kicks < PACKETS, "{} kicks for {PACKETS} packets", g.kicks - kicks);
    }

    #[test]
    fn header_round_trip() {
        let h =
            Hdr { src_cid: 3, dst_cid: 2, src_port: 1234, dst_port: 5000, len: 7, typ: 1, op: OP_RW, flags: 0, buf_alloc: 9, fwd_cnt: 4 };
        assert_eq!(Hdr::parse(&h.encode()), Some(h));
        assert!(Hdr::parse(&[0u8; 10]).is_none());
    }
}
