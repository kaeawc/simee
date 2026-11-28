//! virtio-snd (virtio 1.2 §5.14): the sound card.
//!
//! One output and one input PCM stream (a single ALSA card, `pcmC0D0p` / `pcmC0D0c`), no jacks,
//! a stereo channel map per stream. Playback goes to the host's default output and capture comes
//! from its default input (with `-allow-host-audio`; silence otherwise, as stock's) through
//! [`crate::audio`]; with `-no-audio` both are silent and no host device is opened. A host
//! stream is open only while the guest's stream runs.
//!
//! Buffers complete at the stream's nominal rate on the host's clock, whatever the host device
//! does, because the guest's audio HAL takes its timing from blocking PCM writes and reads:
//! completing buffers instantly would make AudioFlinger spin, and tying them to a host device
//! would stall the guest when that device stalls.
//!
//! The ranchu (phone, Wear, TV) audio HAL plays through card 0, as on stock, where the card is
//! virtio-snd on PCI; the Automotive one (`audio.primary.caremu`) calls `mixer_open(0)` and then
//! `mixer_get_name()` without a NULL check, so it crashes without the card.
//!
//! [`Pcm`] is the device's logic (control requests, stream states, buffer pacing) over opaque
//! buffer handles; [`Snd`] connects it to the virtqueues and the host.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use emu_hvf::GuestRam;

use super::queue::{Chain, Queue};
use super::{Activation, INT_USED_RING, Interrupt, VirtioDevice};
use crate::audio::host::{Command, Direction, Host};
use crate::audio::tap::{PcmFormat, Tap};
use crate::audio::{self, Format, Options, Reader, Ring};

pub const ID_SOUND: u32 = 25;

const QUEUE_CONTROL: usize = 0;
const QUEUE_TX: usize = 2;
const QUEUE_RX: usize = 3;

// Request codes.
const R_JACK_INFO: u32 = 1;
const R_JACK_REMAP: u32 = 2;
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;
const R_CHMAP_INFO: u32 = 0x0200;

// Status codes.
pub const S_OK: u32 = 0x8000;
pub const S_BAD_MSG: u32 = 0x8001;
pub const S_NOT_SUPP: u32 = 0x8002;

pub const D_OUTPUT: u8 = 0;
pub const D_INPUT: u8 = 1;

// `VIRTIO_SND_PCM_FMT_*` bits offered, with their sample sizes in bytes. Only signed and float
// formats, whose silence is all zero bytes.
const FORMATS: &[(u8, u32)] = &[(5, 2), (15, 4), (17, 4), (19, 4)]; // S16, S24 (in 32), S32, FLOAT
// `VIRTIO_SND_PCM_RATE_*` bits offered (what the goldfish and car audio HALs ask for).
const RATES: &[(u8, u32)] = &[(1, 8000), (2, 11025), (3, 16000), (4, 22050), (5, 32000), (6, 44100), (7, 48000)];
const CHANNELS: (u8, u8) = (1, 2);
const CHMAP_FL: u8 = 3;
const CHMAP_FR: u8 = 4;

const PCM_INFO_SIZE: usize = 32;
const CHMAP_INFO_SIZE: usize = 24;
/// `virtio_snd_pcm_status`: status and latency, at the end of every I/O buffer.
pub const IO_STATUS_SIZE: u32 = 8;
/// Seconds of audio a ring holds: more than any guest buffer, so only a stalled host drops. One
/// I/O buffer carries at most this much ([`Params::max_io_bytes`]).
const RING_SECONDS: u32 = 1;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Accepted `PCM_SET_PARAMS` for one stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub buffer_bytes: u32,
    pub period_bytes: u32,
    pub channels: u8,
    pub format: u8,
    pub rate: u8,
}

impl Params {
    /// Frames per second.
    pub fn hz(&self) -> u32 {
        RATES.iter().find(|(r, _)| *r == self.rate).map_or(48_000, |(_, h)| *h)
    }

    /// Bytes per frame.
    pub fn frame_bytes(&self) -> u32 {
        FORMATS.iter().find(|(f, _)| *f == self.format).map_or(2, |(_, s)| *s) * u32::from(self.channels)
    }

    /// Bytes per second.
    fn byte_rate(&self) -> u64 {
        u64::from(self.frame_bytes()) * u64::from(self.hz())
    }

    /// How long `bytes` of audio last.
    fn duration(&self, bytes: u32) -> Duration {
        Duration::from_nanos(u64::from(bytes) * 1_000_000_000 / self.byte_rate().max(1))
    }

    /// The most audio bytes one I/O buffer may carry: the stream's buffer, and no more than its
    /// ring holds. The guest sizes its chains (64 descriptors of up to 4 GiB each), so anything
    /// longer completes as a bad message and is never read, decoded or allocated for.
    pub fn max_io_bytes(&self) -> u32 {
        self.buffer_bytes.min(self.hz() * RING_SECONDS * self.frame_bytes())
    }

    /// The stream's buffer in frames.
    fn buffer_frames(&self) -> usize {
        (self.buffer_bytes / self.frame_bytes().max(1)) as usize
    }
}

/// An I/O buffer the guest queued: an opaque handle and its audio byte count.
#[derive(Debug, PartialEq, Eq)]
pub struct Io<T> {
    pub handle: T,
    pub bytes: u32,
}

/// A buffer to return to the guest with its status and the latency to report (bytes still
/// queued behind it).
#[derive(Debug, PartialEq, Eq)]
pub struct Done<T> {
    pub handle: T,
    pub status: u32,
    pub latency: u32,
}

#[derive(Debug)]
struct Stream<T> {
    direction: u8,
    params: Option<Params>,
    prepared: bool,
    running: bool,
    /// When the audio consumed so far ends (running streams only).
    cursor: Instant,
    pending: VecDeque<Io<T>>,
}

impl<T> Stream<T> {
    fn complete(&mut self, io: Io<T>, status: u32, done: &mut Vec<Done<T>>) {
        let latency = self.pending.iter().map(|p| p.bytes).sum();
        done.push(Done { handle: io.handle, status, latency });
    }

    /// Returns every queued buffer now (STOP, RELEASE: the driver waits for them).
    fn flush(&mut self, done: &mut Vec<Done<T>>) {
        while let Some(io) = self.pending.pop_front() {
            self.complete(io, S_OK, done);
        }
    }
}

/// The sound card's streams and control logic, over buffer handles `T`.
#[derive(Debug)]
pub struct Pcm<T> {
    streams: Vec<Stream<T>>,
}

impl<T> Pcm<T> {
    pub fn new(now: Instant) -> Pcm<T> {
        let stream = |direction| Stream { direction, params: None, prepared: false, running: false, cursor: now, pending: VecDeque::new() };
        Pcm { streams: vec![stream(D_OUTPUT), stream(D_INPUT)] }
    }

    /// The accepted parameters of stream `id`.
    pub fn params(&self, id: usize) -> Option<Params> {
        self.streams.get(id).and_then(|s| s.params)
    }

    /// Stream `id`'s direction ([`D_OUTPUT`] or [`D_INPUT`]).
    pub fn direction(&self, id: usize) -> Option<u8> {
        self.streams.get(id).map(|s| s.direction)
    }

    /// `virtio_snd_config`: jacks, streams, chmaps.
    pub fn config(&self) -> [u8; 12] {
        let mut c = [0u8; 12];
        c[4..8].copy_from_slice(&(self.streams.len() as u32).to_le_bytes());
        c[8..12].copy_from_slice(&(self.streams.len() as u32).to_le_bytes());
        c
    }

    fn pcm_info(&self, id: usize) -> Vec<u8> {
        let mut b = vec![0u8; PCM_INFO_SIZE];
        // hda_fn_nid 0 for both: one ALSA PCM device with a playback and a capture substream.
        let formats: u64 = FORMATS.iter().fold(0, |m, (f, _)| m | 1 << f);
        let rates: u64 = RATES.iter().fold(0, |m, (r, _)| m | 1 << r);
        b[8..16].copy_from_slice(&formats.to_le_bytes());
        b[16..24].copy_from_slice(&rates.to_le_bytes());
        b[24] = self.streams[id].direction;
        b[25] = CHANNELS.0;
        b[26] = CHANNELS.1;
        b
    }

    fn chmap_info(&self, id: usize) -> Vec<u8> {
        let mut b = vec![0u8; CHMAP_INFO_SIZE];
        b[4] = self.streams[id].direction;
        b[5] = 2;
        b[6] = CHMAP_FL;
        b[7] = CHMAP_FR;
        b
    }

    /// Handles one control request; returns the response (status, then any info structures).
    /// Buffers that a STOP or RELEASE completes go to `done`.
    pub fn control(&mut self, req: &[u8], now: Instant, done: &mut Vec<Done<T>>) -> Vec<u8> {
        let status = |s: u32| s.to_le_bytes().to_vec();
        let Some(code) = le32(req, 0) else { return status(S_BAD_MSG) };
        match code {
            R_PCM_INFO | R_CHMAP_INFO | R_JACK_INFO => {
                let (Some(start), Some(count), Some(size)) = (le32(req, 4), le32(req, 8), le32(req, 12)) else {
                    return status(S_BAD_MSG);
                };
                let items = if code == R_JACK_INFO { 0 } else { self.streams.len() };
                let (start, count, size) = (start as usize, count as usize, size as usize);
                if size == 0 || start.checked_add(count).is_none_or(|end| end > items) {
                    return status(S_BAD_MSG);
                }
                let mut out = status(S_OK);
                for id in start..start + count {
                    let mut item = if code == R_PCM_INFO { self.pcm_info(id) } else { self.chmap_info(id) };
                    // Each item takes the driver's `size`: padded, or cut to it.
                    item.resize(size, 0);
                    out.extend_from_slice(&item);
                }
                out
            }
            R_JACK_REMAP => status(S_BAD_MSG),
            R_PCM_SET_PARAMS | R_PCM_PREPARE | R_PCM_RELEASE | R_PCM_START | R_PCM_STOP => {
                let Some(s) = le32(req, 4).and_then(|id| self.streams.get_mut(id as usize)) else { return status(S_BAD_MSG) };
                status(match code {
                    R_PCM_SET_PARAMS => {
                        let (Some(buffer_bytes), Some(period_bytes), Some(&[channels, format, rate])) =
                            (le32(req, 8), le32(req, 12), req.get(20..23))
                        else {
                            return status(S_BAD_MSG);
                        };
                        let p = Params { buffer_bytes, period_bytes, channels, format, rate };
                        if s.running {
                            S_BAD_MSG
                        } else if !(CHANNELS.0..=CHANNELS.1).contains(&channels)
                            || !FORMATS.iter().any(|(f, _)| *f == format)
                            || !RATES.iter().any(|(r, _)| *r == rate)
                            || period_bytes == 0
                            || buffer_bytes < period_bytes
                        {
                            S_NOT_SUPP
                        } else {
                            s.params = Some(p);
                            s.prepared = false;
                            S_OK
                        }
                    }
                    R_PCM_PREPARE if s.running || s.params.is_none() => S_BAD_MSG,
                    R_PCM_PREPARE => {
                        s.prepared = true;
                        S_OK
                    }
                    R_PCM_START if s.running || !s.prepared => S_BAD_MSG,
                    R_PCM_START => {
                        s.running = true;
                        s.cursor = now;
                        S_OK
                    }
                    R_PCM_STOP if !s.running => S_BAD_MSG,
                    R_PCM_STOP => {
                        s.running = false;
                        s.flush(done);
                        S_OK
                    }
                    // RELEASE
                    _ if s.running => S_BAD_MSG,
                    _ => {
                        s.prepared = false;
                        s.flush(done);
                        S_OK
                    }
                })
            }
            _ => status(S_NOT_SUPP),
        }
    }

    /// Whether stream `stream_id` takes an I/O buffer of `direction` carrying `bytes` of audio:
    /// its direction, with parameters set, and no longer than [`Params::max_io_bytes`].
    pub fn accepts(&self, stream_id: u32, direction: u8, bytes: u64) -> bool {
        self.streams
            .get(stream_id as usize)
            .and_then(|s| s.params.filter(|_| s.direction == direction))
            .is_some_and(|p| bytes <= u64::from(p.max_io_bytes()))
    }

    /// Queues an I/O buffer for `stream_id`, which must be a stream of `direction` (tx buffers
    /// go to the output stream, rx buffers to the input one). Bad buffers complete at once.
    pub fn queue(&mut self, stream_id: u32, direction: u8, io: Io<T>, now: Instant, done: &mut Vec<Done<T>>) {
        let accepted = self.accepts(stream_id, direction, u64::from(io.bytes));
        match self.streams.get_mut(stream_id as usize) {
            Some(s) if accepted => {
                if s.pending.is_empty() && s.running && s.cursor < now {
                    // Restarting after an underrun: time runs from now, not from the gap.
                    s.cursor = now;
                }
                s.pending.push_back(io);
            }
            _ => done.push(Done { handle: io.handle, status: S_BAD_MSG, latency: 0 }),
        }
    }

    /// Completes the buffers whose audio has been played (or captured) by `now`; returns when
    /// the next one will be.
    pub fn tick(&mut self, now: Instant, done: &mut Vec<Done<T>>) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        for s in &mut self.streams {
            let Some(p) = s.params.filter(|_| s.running) else { continue };
            while let Some(bytes) = s.pending.front().map(|io| io.bytes) {
                let length = p.duration(bytes);
                let end = s.cursor + length;
                if end > now {
                    next = Some(next.map_or(end, |n| n.min(end)));
                    break;
                }
                // More than a buffer late (the host descheduled this thread): complete one now
                // and the rest at the stream's rate from here, never the whole queue at once.
                // Seen on API 36: after a burst that returned all four queued periods, the guest
                // driver queued nothing more and every write timed out (EIO) until the HAL
                // reopened the stream.
                s.cursor = if now.saturating_duration_since(end) > length { now } else { end };
                if let Some(io) = s.pending.pop_front() {
                    s.complete(io, S_OK, done);
                }
            }
        }
        next
    }
}

enum Msg {
    Kick,
    Stop,
}

pub struct Snd {
    opts: Options,
    tap: Arc<Tap>,
    tx: Sender<Msg>,
    rx: Option<Receiver<Msg>>,
    worker: Option<JoinHandle<Receiver<Msg>>>,
}

impl Snd {
    /// A card that plays and records on the host as `opts` allow.
    pub fn new(opts: Options) -> Snd {
        Snd::with_tap(opts, Arc::default())
    }

    /// A card whose playback `tap` can copy, with or without a host output.
    pub fn with_tap(opts: Options, tap: Arc<Tap>) -> Snd {
        let (tx, rx) = channel();
        tap.set_card(true);
        Snd { opts, tap, tx, rx: Some(rx), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.tx.send(Msg::Stop);
            if let Ok(rx) = h.join() {
                self.rx = Some(rx);
            }
        }
    }
}

impl Default for Snd {
    /// A silent card.
    fn default() -> Self {
        Snd::new(Options::default())
    }
}

impl Drop for Snd {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Snd {
    fn device_id(&self) -> u32 {
        ID_SOUND
    }
    fn features(&self) -> u64 {
        0
    }
    fn num_queues(&self) -> usize {
        4
    }
    fn queue_max_size(&self) -> u16 {
        64
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cfg = Pcm::<()>::new(Instant::now()).config();
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let q = |i: usize| act.queues.get(i).copied().flatten().map(|c| Queue::new(c, false));
        let control = q(QUEUE_CONTROL).ok_or("sound control queue not enabled")?;
        self.stop_worker();
        let rx = self.rx.take().ok_or("sound worker receiver missing")?;
        // No host thread at all for a silent card (`-no-audio`).
        let host = (self.opts.output || self.opts.input).then(|| Box::new(Host::new(self.opts.verbose)) as Box<dyn Sink>);
        let mut w = Worker::new(act.mem, act.irq, control, [q(QUEUE_TX), q(QUEUE_RX)], self.opts, host);
        w.tap = self.tap.clone();
        let h = std::thread::Builder::new()
            .name("virtio-snd".into())
            .spawn(move || {
                crate::device_thread();
                let mut deadline: Option<Instant> = None;
                loop {
                    let msg = match deadline {
                        Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
                        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    match msg {
                        Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                        Ok(Msg::Kick) | Err(RecvTimeoutError::Timeout) => deadline = w.run(),
                    }
                }
                rx
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some(h);
        Ok(())
    }
    fn notify(&mut self, _queue: u16) {
        let _ = self.tx.send(Msg::Kick);
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

fn readable_len(chain: &Chain) -> u64 {
    chain.readable().map(|d| u64::from(d.len)).sum()
}

fn writable_len(chain: &Chain) -> u64 {
    chain.writable().map(|d| u64::from(d.len)).sum()
}

/// Writes `data` into the last `data.len()` writable bytes of `chain` (the status that ends an
/// I/O buffer) and nothing before them; returns the bytes written.
fn write_tail(chain: &Chain, mem: &GuestRam, mut data: &[u8]) -> u32 {
    let Some(mut skip) = writable_len(chain).checked_sub(data.len() as u64) else { return 0 };
    let mut written = 0;
    for d in chain.writable() {
        let len = u64::from(d.len);
        if skip >= len {
            skip -= len;
            continue;
        }
        let take = ((len - skip) as usize).min(data.len());
        let Some(addr) = d.addr.checked_add(skip) else { break };
        if take == 0 || mem.write(addr, &data[..take]).is_err() {
            break;
        }
        (data, written, skip) = (&data[take..], written + take as u32, 0);
    }
    written
}

/// How the worker reaches the host: the real one, or (in tests) a log of commands.
trait Sink: Send {
    fn send(&mut self, cmd: Command);
}

impl Sink for Host {
    fn send(&mut self, cmd: Command) {
        Host::send(self, cmd);
    }
}

/// A stream's host side while the guest has it prepared or running.
#[derive(Default)]
struct Link {
    /// Output: the guest's frames on their way to the host. Input: the host's on their way in.
    ring: Option<Arc<Ring>>,
    /// Input: reads the ring at the guest's rate.
    reader: Option<Reader>,
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    control: Queue,
    txq: Option<Queue>,
    rxq: Option<Queue>,
    pcm: Pcm<(usize, Chain)>,
    opts: Options,
    host: Option<Box<dyn Sink>>,
    /// The capture tap on playback; idle (one atomic load per buffer) unless the console starts it.
    tap: Arc<Tap>,
    links: [Link; 2],
    /// Reused for every buffer: guest bytes and stereo frames.
    bytes: Vec<u8>,
    frames: Vec<audio::Frame>,
}

impl Worker {
    /// `io`: the tx and rx queues.
    fn new(
        mem: Arc<GuestRam>,
        irq: Arc<Interrupt>,
        control: Queue,
        io: [Option<Queue>; 2],
        opts: Options,
        host: Option<Box<dyn Sink>>,
    ) -> Worker {
        let [txq, rxq] = io;
        Worker {
            mem,
            irq,
            control,
            txq,
            rxq,
            pcm: Pcm::new(Instant::now()),
            opts,
            host,
            tap: Arc::default(),
            links: Default::default(),
            bytes: Vec::new(),
            frames: Vec::new(),
        }
    }

    fn enabled(&self, direction: u8) -> bool {
        self.host.is_some() && if direction == D_OUTPUT { self.opts.output } else { self.opts.input }
    }

    fn send(&mut self, cmd: Command) {
        if let Some(h) = self.host.as_mut() {
            h.send(cmd);
        }
    }

    /// Follows a control request that succeeded onto the host: a stream that starts opens its
    /// host stream, one that stops or is released closes it, new parameters start a new ring.
    fn follow(&mut self, req: &[u8], resp: &[u8]) {
        let (Some(code), Some(id), Some(S_OK)) = (le32(req, 0), le32(req, 4), le32(resp, 0)) else { return };
        let id = id as usize;
        let (Some(direction), Some(params)) = (self.pcm.direction(id), self.pcm.params(id)) else { return };
        if self.opts.verbose {
            let what = match code {
                R_PCM_SET_PARAMS => "set params",
                R_PCM_PREPARE => "prepare",
                R_PCM_START => "start",
                R_PCM_STOP => "stop",
                R_PCM_RELEASE => "release",
                _ => return,
            };
            eprintln!(
                "emu: snd: stream {id} {what} ({} Hz, {} ch, format {}, period {} B, buffer {} B)",
                params.hz(),
                params.channels,
                params.format,
                params.period_bytes,
                params.buffer_bytes
            );
        }
        if !self.enabled(direction) {
            return;
        }
        let dir = if direction == D_OUTPUT { Direction::Output } else { Direction::Input };
        match code {
            // STOP too: frames left in a stopped stream's ring would play (or be read) late
            // after the next START.
            R_PCM_SET_PARAMS | R_PCM_PREPARE | R_PCM_RELEASE | R_PCM_STOP => {
                self.send(Command::Close(dir));
                self.links[id] = Link::default();
            }
            R_PCM_START => {
                let hz = params.hz();
                let buffer_frames = params.buffer_frames();
                let ring = if direction == D_OUTPUT {
                    // Frames queued before START (ALSA prefills) are already in it.
                    self.links[id].ring.get_or_insert_with(|| Arc::new(Ring::new((hz * RING_SECONDS) as usize, hz))).clone()
                } else {
                    // The host sets the rate when its stream opens; until then capture is silence.
                    let ring = Arc::new(Ring::new((hz * RING_SECONDS) as usize, 0));
                    // Start once 20 ms is in; keep at most 200 ms (or the guest's buffer) of latency.
                    let prime = hz as usize / 50;
                    let reader = Reader::new(hz, prime, audio::max_latency(hz, buffer_frames, prime));
                    self.links[id] = Link { ring: Some(ring.clone()), reader: Some(reader) };
                    ring
                };
                self.send(Command::Open { direction: dir, ring, buffer_frames });
            }
            _ => {}
        }
    }

    /// Playback: the guest's audio in `chain` (after its 4-byte header) into the stream's ring.
    /// Reads at most the header and [`Params::max_io_bytes`].
    fn play(&mut self, id: usize, chain: &Chain) {
        let Some(p) = self.pcm.params(id).filter(|_| self.pcm.direction(id) == Some(D_OUTPUT)) else { return };
        let (tapping, playing) = (self.tap.is_active(), self.enabled(D_OUTPUT));
        if !tapping && !playing {
            return;
        }
        let Some(format) = Format::from_virtio(p.format) else { return };
        let max = 4 + p.max_io_bytes() as usize;
        if chain.read_all_into(&self.mem, max, &mut self.bytes).is_err() || self.bytes.len() <= 4 {
            return;
        }
        if tapping {
            self.tap.offer(PcmFormat { rate: p.hz(), channels: p.channels, format }, &self.bytes[4..]);
        }
        if !playing {
            return;
        }
        let ring = self.links[id].ring.get_or_insert_with(|| Arc::new(Ring::new((p.hz() * RING_SECONDS) as usize, p.hz())));
        self.frames.clear();
        audio::decode(format, usize::from(p.channels), &self.bytes[4..], &mut self.frames);
        ring.push_all(&self.frames);
    }

    /// Capture: `len` bytes (at most [`Params::max_io_bytes`]) of the stream's audio from the
    /// host into `self.bytes`, or silence.
    fn record(&mut self, id: usize, len: usize) {
        self.bytes.clear();
        let p = self.pcm.params(id);
        let len = len.min(p.map_or(0, |p| p.max_io_bytes() as usize));
        let link = &mut self.links[id];
        if let (Some(p), Some(ring), Some(reader)) = (p, link.ring.as_ref(), link.reader.as_mut())
            && let Some(format) = Format::from_virtio(p.format)
        {
            let frames = len / p.frame_bytes().max(1) as usize;
            self.frames.clear();
            self.frames.resize(frames, [0.0; 2]);
            reader.read(ring, &mut self.frames);
            audio::encode(format, usize::from(p.channels), &self.frames, &mut self.bytes);
        }
        self.bytes.resize(len, 0);
    }

    /// Serves every queue; returns when the next buffer is due.
    fn run(&mut self) -> Option<Instant> {
        let now = Instant::now();
        let mut done = Vec::new();
        let mut control_used = false;
        while let Ok(Some(chain)) = self.control.pop(&self.mem) {
            let req = chain.read_all(&self.mem, 64).unwrap_or_default();
            let resp = self.pcm.control(&req, now, &mut done);
            self.follow(&req, &resp);
            let n = chain.write_all(&self.mem, &resp).unwrap_or(0);
            let _ = self.control.add_used(&self.mem, chain.head, n);
            control_used = true;
        }
        for (queue, direction) in [(QUEUE_TX, D_OUTPUT), (QUEUE_RX, D_INPUT)] {
            loop {
                let q = if queue == QUEUE_TX { self.txq.as_mut() } else { self.rxq.as_mut() };
                let Some(Ok(Some(chain))) = q.map(|q| q.pop(&self.mem)) else { break };
                // The header (`virtio_snd_pcm_xfer`: the stream id) leads the readable part; the
                // status ends the writable part. Playback data is readable, capture writable.
                let header = chain.read_all(&self.mem, 4).unwrap_or_default();
                // In u64: 64 guest-sized lengths overflow a u32.
                let (readable, writable) = (readable_len(&chain), writable_len(&chain));
                let status = u64::from(IO_STATUS_SIZE);
                let bytes = if direction == D_OUTPUT { readable.saturating_sub(4) } else { writable.saturating_sub(status) };
                match le32(&header, 0) {
                    // Oversized buffers are refused before anything is read from them.
                    Some(id) if writable >= status && self.pcm.accepts(id, direction, bytes) => {
                        if direction == D_OUTPUT {
                            self.play(id as usize, &chain);
                        }
                        let bytes = u32::try_from(bytes).unwrap_or(u32::MAX);
                        self.pcm.queue(id, direction, Io { handle: (queue, chain), bytes }, now, &mut done);
                    }
                    _ => done.push(Done { handle: (queue, chain), status: S_BAD_MSG, latency: 0 }),
                }
            }
        }
        let next = self.pcm.tick(now, &mut done);
        let (mut tx_used, mut rx_used) = (false, false);
        for d in done {
            let (queue, chain) = d.handle;
            // The status goes in the last 8 writable bytes; capture data before it.
            let len = writable_len(&chain).saturating_sub(u64::from(IO_STATUS_SIZE));
            let mut status = [0u8; IO_STATUS_SIZE as usize];
            status[..4].copy_from_slice(&d.status.to_le_bytes());
            status[4..].copy_from_slice(&d.latency.to_le_bytes());
            let cap = self.pcm.params(1).map_or(0, |p| u64::from(p.max_io_bytes()));
            let n = if queue == QUEUE_RX && d.status == S_OK && len <= cap {
                self.record(1, len as usize);
                self.bytes.extend_from_slice(&status);
                chain.write_all(&self.mem, &self.bytes).unwrap_or(0)
            } else {
                // Playback, or a buffer refused: only the status, whatever the guest's lengths.
                write_tail(&chain, &self.mem, &status)
            };
            let q = if queue == QUEUE_TX { self.txq.as_mut() } else { self.rxq.as_mut() };
            if let Some(q) = q {
                let _ = q.add_used(&self.mem, chain.head, n);
                if queue == QUEUE_TX { tx_used = true } else { rx_used = true }
            }
        }
        let notify = |q: Option<&mut Queue>, used: bool, mem: &GuestRam| used && q.is_some_and(|q| q.should_notify(mem));
        let control = control_used && self.control.should_notify(&self.mem);
        if control | notify(self.txq.as_mut(), tx_used, &self.mem) | notify(self.rxq.as_mut(), rx_used, &self.mem) {
            self.irq.raise(INT_USED_RING);
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn set_params(stream: u32, buffer: u32, period: u32, channels: u8, format: u8, rate: u8) -> Vec<u8> {
        let mut r = req(&[R_PCM_SET_PARAMS, stream, buffer, period, 0]);
        r.extend_from_slice(&[channels, format, rate, 0]);
        r
    }

    fn status(resp: &[u8]) -> u32 {
        le32(resp, 0).expect("status")
    }

    #[test]
    fn describes_one_output_and_one_input() {
        let mut pcm = Pcm::<u32>::new(Instant::now());
        assert_eq!(pcm.config(), *b"\0\0\0\0\x02\0\0\0\x02\0\0\0");
        let now = Instant::now();
        let mut done = Vec::new();
        let info = pcm.control(&req(&[R_PCM_INFO, 0, 2, 32]), now, &mut done);
        assert_eq!((status(&info), info.len()), (S_OK, 4 + 64));
        let out = &info[4..36];
        let formats = u64::from_le_bytes(out[8..16].try_into().expect("8"));
        let rates = u64::from_le_bytes(out[16..24].try_into().expect("8"));
        assert_eq!((formats & 1 << 5 != 0, rates & 1 << 7 != 0), (true, true), "S16 at 48 kHz");
        assert_eq!((out[24], out[25], out[26]), (D_OUTPUT, 1, 2));
        assert_eq!(info[36 + 24], D_INPUT);
        // Items take the driver's size.
        assert_eq!(pcm.control(&req(&[R_PCM_INFO, 1, 1, 40]), now, &mut done).len(), 4 + 40);
        let chmap = pcm.control(&req(&[R_CHMAP_INFO, 0, 1, 24]), now, &mut done);
        assert_eq!((status(&chmap), &chmap[8..12]), (S_OK, &[D_OUTPUT, 2, CHMAP_FL, CHMAP_FR][..]));
        assert_eq!(status(&pcm.control(&req(&[R_PCM_INFO, 1, 2, 32]), now, &mut done)), S_BAD_MSG, "past the last stream");
        assert_eq!(status(&pcm.control(&req(&[R_JACK_INFO, 0, 1, 24]), now, &mut done)), S_BAD_MSG, "no jacks");
        assert_eq!(status(&pcm.control(&req(&[R_JACK_INFO, 0, 0, 24]), now, &mut done)), S_OK);
        assert_eq!(status(&pcm.control(&req(&[0x0300]), now, &mut done)), S_NOT_SUPP, "no controls");
        assert_eq!(status(&pcm.control(&[1, 0], now, &mut done)), S_BAD_MSG, "short header");
        assert!(done.is_empty());
    }

    #[test]
    fn follows_the_stream_state_machine() {
        let mut pcm = Pcm::<u32>::new(Instant::now());
        let now = Instant::now();
        let mut done = Vec::new();
        let mut call = |r: Vec<u8>| status(&pcm.control(&r, now, &mut done));
        assert_eq!(call(req(&[R_PCM_PREPARE, 0])), S_BAD_MSG, "no parameters yet");
        assert_eq!(call(set_params(0, 3840, 960, 2, 5, 7)), S_OK);
        assert_eq!(call(set_params(0, 3840, 960, 6, 5, 7)), S_NOT_SUPP, "six channels");
        assert_eq!(call(set_params(0, 3840, 960, 2, 4, 7)), S_NOT_SUPP, "U8");
        assert_eq!(call(set_params(0, 3840, 960, 2, 5, 13)), S_NOT_SUPP, "384 kHz");
        assert_eq!(call(set_params(0, 100, 960, 2, 5, 7)), S_NOT_SUPP, "buffer smaller than a period");
        assert_eq!(call(set_params(2, 3840, 960, 2, 5, 7)), S_BAD_MSG, "no stream 2");
        assert_eq!(call(req(&[R_PCM_START, 0])), S_BAD_MSG, "not prepared");
        assert_eq!(call(req(&[R_PCM_PREPARE, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_START, 0])), S_OK);
        assert_eq!(call(set_params(0, 3840, 960, 2, 5, 7)), S_BAD_MSG, "running");
        assert_eq!(call(req(&[R_PCM_RELEASE, 0])), S_BAD_MSG, "running");
        assert_eq!(call(req(&[R_PCM_STOP, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_STOP, 0])), S_BAD_MSG, "already stopped");
        assert_eq!(call(req(&[R_PCM_START, 0])), S_OK, "stopped streams start again");
        assert_eq!(call(req(&[R_PCM_STOP, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_RELEASE, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_START, 0])), S_BAD_MSG, "released");
    }

    #[test]
    fn paces_playback_and_returns_silent_capture() {
        let t0 = Instant::now();
        let mut pcm = Pcm::<u32>::new(t0);
        let mut done = Vec::new();
        // 48 kHz stereo S16: 192 000 bytes/s, so 1920 bytes last 10 ms.
        for id in [0, 1] {
            assert_eq!(status(&pcm.control(&set_params(id, 7680, 1920, 2, 5, 7), t0, &mut done)), S_OK);
            assert_eq!(status(&pcm.control(&req(&[R_PCM_PREPARE, id]), t0, &mut done)), S_OK);
        }
        // Prefilled before START, as ALSA does.
        pcm.queue(0, D_OUTPUT, Io { handle: 1, bytes: 1920 }, t0, &mut done);
        pcm.queue(0, D_OUTPUT, Io { handle: 2, bytes: 1920 }, t0, &mut done);
        assert_eq!(pcm.tick(t0 + Duration::from_secs(1), &mut done), None, "not started: nothing moves");
        assert!(done.is_empty());
        pcm.control(&req(&[R_PCM_START, 0]), t0, &mut done);
        assert_eq!(pcm.tick(t0 + Duration::from_millis(5), &mut done), Some(t0 + Duration::from_millis(10)));
        assert!(done.is_empty());
        assert_eq!(pcm.tick(t0 + Duration::from_millis(10), &mut done), Some(t0 + Duration::from_millis(20)));
        assert_eq!(done, vec![Done { handle: 1, status: S_OK, latency: 1920 }]);
        done.clear();

        // Capture runs on its own clock: 960 bytes are 5 ms.
        pcm.control(&req(&[R_PCM_START, 1]), t0, &mut done);
        pcm.queue(1, D_INPUT, Io { handle: 7, bytes: 960 }, t0, &mut done);
        assert_eq!(pcm.tick(t0 + Duration::from_millis(5), &mut done), Some(t0 + Duration::from_millis(20)));
        assert_eq!(done, vec![Done { handle: 7, status: S_OK, latency: 0 }]);
        done.clear();
        pcm.tick(t0 + Duration::from_millis(20), &mut done);
        assert_eq!(done, vec![Done { handle: 2, status: S_OK, latency: 0 }]);
        done.clear();

        // Wrong direction or unknown stream: rejected at once. STOP flushes what's queued.
        pcm.queue(1, D_OUTPUT, Io { handle: 8, bytes: 4 }, t0, &mut done);
        pcm.queue(9, D_OUTPUT, Io { handle: 9, bytes: 4 }, t0, &mut done);
        assert_eq!(done.iter().map(|d| (d.handle, d.status)).collect::<Vec<_>>(), [(8, S_BAD_MSG), (9, S_BAD_MSG)]);
        done.clear();
        pcm.queue(0, D_OUTPUT, Io { handle: 10, bytes: 1920 }, t0, &mut done);
        pcm.control(&req(&[R_PCM_STOP, 0]), t0, &mut done);
        assert_eq!(done, vec![Done { handle: 10, status: S_OK, latency: 0 }]);

        // After an underrun, time restarts when audio arrives again.
        done.clear();
        pcm.control(&req(&[R_PCM_START, 0]), t0, &mut done);
        let late = t0 + Duration::from_secs(5);
        pcm.queue(0, D_OUTPUT, Io { handle: 11, bytes: 1920 }, late, &mut done);
        assert_eq!(pcm.tick(late, &mut done), Some(late + Duration::from_millis(10)));
        assert!(done.is_empty(), "not completed instantly");
    }

    #[test]
    fn a_stalled_host_never_returns_the_whole_queue_at_once() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut pcm = Pcm::<u32>::new(t0);
        let mut done = Vec::new();
        // 48 kHz stereo S16, 10 ms periods; the driver keeps four queued.
        pcm.control(&set_params(0, 7680, 1920, 2, 5, 7), t0, &mut done);
        pcm.control(&req(&[R_PCM_PREPARE, 0]), t0, &mut done);
        for h in 1..=4 {
            pcm.queue(0, D_OUTPUT, Io { handle: h, bytes: 1920 }, t0, &mut done);
        }
        pcm.control(&req(&[R_PCM_START, 0]), t0, &mut done);
        // The worker wakes 300 ms late: one period completes now, the rest at the rate from here.
        assert_eq!(pcm.tick(ms(300), &mut done), Some(ms(310)));
        assert_eq!(done.iter().map(|d| d.handle).collect::<Vec<_>>(), [1]);
        done.clear();
        // Up to a period late still catches up: both overdue periods complete.
        assert_eq!(pcm.tick(ms(320), &mut done), Some(ms(330)));
        assert_eq!(done.iter().map(|d| d.handle).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn serves_the_virtqueues() {
        use crate::irq::TestLine;
        use crate::virtio::queue::tests::{Driver, RAM_BASE};
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        // Three queues, each laid out in its own 16 KiB.
        let driver = |n: u64| {
            let mut d = Driver::new(8);
            for a in [&mut d.cfg.desc, &mut d.cfg.avail, &mut d.cfg.used] {
                *a += n * 0x4000;
            }
            d
        };
        let (mut ctl, mut txd, mut rxd) = (driver(0), driver(1), driver(2));
        let mut w = Worker::new(
            mem.clone(),
            Arc::new(Interrupt::new(line.clone())),
            Queue::new(ctl.cfg, false),
            [Some(Queue::new(txd.cfg, false)), Some(Queue::new(rxd.cfg, false))],
            Options::default(),
            None,
        );
        fn buf(i: u64) -> u64 {
            RAM_BASE + 0x8_0000 + i * 0x1000
        }
        fn control(w: &mut Worker, ctl: &mut Driver, i: u64, r: Vec<u8>, resp: u32) {
            w.mem.write(buf(i), &r).expect("request");
            ctl.push(&w.mem, &[(buf(i), r.len() as u32, false), (buf(i) + 0x800, resp, true)]);
            w.run();
        }
        control(&mut w, &mut ctl, 0, req(&[R_PCM_INFO, 0, 2, 32]), 4 + 64);
        assert_eq!((ctl.used(&mem, 0).1, mem.read_u32(buf(0) + 0x800).expect("status")), (68, S_OK));
        assert!(line.level(), "used ring interrupt");
        control(&mut w, &mut ctl, 1, set_params(0, 3840, 1920, 2, 5, 7), 4);
        control(&mut w, &mut ctl, 2, req(&[R_PCM_PREPARE, 0]), 4);
        control(&mut w, &mut ctl, 3, req(&[R_PCM_START, 0]), 4);
        assert_eq!((ctl.used_idx(&mem), mem.read_u32(buf(3) + 0x800).expect("status")), (4, S_OK));

        // 1920 bytes of playback: back after 10 ms, with only the status written.
        mem.write(buf(4), &0u32.to_le_bytes()).expect("xfer header");
        txd.push(&mem, &[(buf(4), 4, false), (buf(5), 1920, false), (buf(4) + 0x800, 8, true)]);
        assert!(w.run().is_some(), "a buffer is due later");
        assert_eq!(txd.used_idx(&mem), 0);
        std::thread::sleep(Duration::from_millis(11));
        w.run();
        assert_eq!((txd.used(&mem, 0).1, mem.read_u32(buf(4) + 0x800).expect("status")), (8, S_OK));

        // Capture on a stream with no parameters: rejected, only the status written, at the end.
        mem.write(buf(6), &1u32.to_le_bytes()).expect("xfer header");
        mem.write(buf(6) + 0x800, &[0xaa; 24]).expect("data");
        rxd.push(&mem, &[(buf(6), 4, false), (buf(6) + 0x800, 16, true), (buf(6) + 0x810, 8, true)]);
        w.run();
        let mut back = [0u8; 24];
        mem.read(buf(6) + 0x800, &mut back).expect("read");
        assert_eq!((rxd.used(&mem, 0).1, &back[..16], le32(&back, 16)), (8, &[0xaa; 16][..], Some(S_BAD_MSG)));
    }

    #[test]
    fn caps_io_buffers_at_the_stream_buffer_and_its_ring() {
        let t0 = Instant::now();
        let mut pcm = Pcm::<u32>::new(t0);
        let mut done = Vec::new();
        // 48 kHz stereo S16, a 3840-byte buffer: no I/O buffer may carry more.
        pcm.control(&set_params(0, 3840, 1920, 2, 5, 7), t0, &mut done);
        assert_eq!(pcm.params(0).expect("params").max_io_bytes(), 3840);
        assert!(pcm.accepts(0, D_OUTPUT, 3840) && !pcm.accepts(0, D_OUTPUT, 3841));
        assert!(!pcm.accepts(0, D_INPUT, 4) && !pcm.accepts(1, D_INPUT, 4) && !pcm.accepts(9, D_OUTPUT, 4));
        // A guest buffer longer than the ring (1 s): the ring's size bounds it.
        pcm.control(&set_params(1, u32::MAX, 960, 2, 5, 7), t0, &mut done);
        assert_eq!(pcm.params(1).expect("params").max_io_bytes(), 192_000);
        assert!(!pcm.accepts(1, D_INPUT, u64::from(u32::MAX) * 64));
        pcm.queue(0, D_OUTPUT, Io { handle: 1, bytes: 3841 }, t0, &mut done);
        pcm.queue(1, D_INPUT, Io { handle: 2, bytes: 192_004 }, t0, &mut done);
        assert_eq!(done.iter().map(|d| (d.handle, d.status)).collect::<Vec<_>>(), [(1, S_BAD_MSG), (2, S_BAD_MSG)]);
    }

    #[test]
    fn refuses_oversized_chains_without_reading_them() {
        use crate::irq::TestLine;
        use crate::virtio::queue::tests::{Driver, RAM_BASE};
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let driver = |n: u64| {
            let mut d = Driver::new(8);
            for a in [&mut d.cfg.desc, &mut d.cfg.avail, &mut d.cfg.used] {
                *a += n * 0x4000;
            }
            d
        };
        let (mut ctl, mut txd, mut rxd) = (driver(0), driver(1), driver(2));
        let mut w = Worker::new(
            mem.clone(),
            Arc::new(Interrupt::new(Arc::new(TestLine::default()))),
            Queue::new(ctl.cfg, false),
            [Some(Queue::new(txd.cfg, false)), Some(Queue::new(rxd.cfg, false))],
            Options { output: true, input: true, verbose: false },
            Some(Box::new(FakeHost::default())),
        );
        fn buf(i: u64) -> u64 {
            RAM_BASE + 0x8_0000 + i * 0x1000
        }
        for (i, r) in
            [set_params(0, 3840, 1920, 2, 5, 7), req(&[R_PCM_PREPARE, 0]), set_params(1, 3840, 960, 2, 5, 7), req(&[R_PCM_PREPARE, 1])]
                .into_iter()
                .enumerate()
        {
            let i = i as u64;
            mem.write(buf(i), &r).expect("request");
            ctl.push(&mem, &[(buf(i), r.len() as u32, false), (buf(i) + 0x800, 4, true)]);
            w.run();
            assert_eq!(mem.read_u32(buf(i) + 0x800).expect("status"), S_OK);
        }
        let status = |at: u64| mem.read_u32(at).expect("status");
        mem.write(buf(4), &0u32.to_le_bytes()).expect("xfer header");
        mem.write(buf(6), &1u32.to_le_bytes()).expect("xfer header");
        // Playback: one byte over the buffer, then two descriptors of nearly 4 GiB (their sum
        // overflows a u32) far outside guest RAM. Capture: nearly 4 GiB to fill.
        txd.push(&mem, &[(buf(4), 4, false), (buf(5), 3841, false), (buf(4) + 0x800, 8, true)]);
        txd.push(&mem, &[(buf(4), 4, false), (1 << 40, u32::MAX, false), (1 << 41, u32::MAX, false), (buf(4) + 0x900, 8, true)]);
        rxd.push(&mem, &[(buf(6), 4, false), (1 << 40, u32::MAX - 8, true), (buf(6) + 0x800, 8, true)]);
        w.run();
        assert_eq!((txd.used_idx(&mem), rxd.used_idx(&mem)), (2, 1), "completed at once");
        assert_eq!([txd.used(&mem, 0).1, txd.used(&mem, 1).1, rxd.used(&mem, 0).1], [8, 8, 8], "only the status written");
        assert_eq!([status(buf(4) + 0x800), status(buf(4) + 0x900), status(buf(6) + 0x800)], [S_BAD_MSG; 3]);
        assert!(w.bytes.capacity() <= 4 + 3840 && w.frames.capacity() <= 960, "nothing read or allocated for them");
        assert!(w.links[0].ring.is_none(), "nothing reached the ring");
    }

    type Sent = (String, Option<Arc<Ring>>);

    /// A host that records what the card asks of it.
    #[derive(Clone, Default)]
    struct FakeHost(Arc<std::sync::Mutex<Vec<Sent>>>);

    impl Sink for FakeHost {
        fn send(&mut self, cmd: Command) {
            let entry = match cmd {
                Command::Open { direction: d, ring, .. } => (format!("open {d:?}"), Some(ring)),
                Command::Close(d) => (format!("close {d:?}"), None),
            };
            self.0.lock().expect("log").push(entry);
        }
    }

    impl FakeHost {
        fn take(&self) -> Vec<Sent> {
            std::mem::take(&mut *self.0.lock().expect("log"))
        }
        fn names(&self) -> Vec<String> {
            self.take().into_iter().map(|(n, _)| n).collect()
        }
    }

    /// A worker with a playback stream prepared (48 kHz stereo S16), and the driver of its tx queue.
    fn playback_worker(opts: Options, host: Option<Box<dyn Sink>>, tap: Arc<Tap>) -> (Worker, crate::virtio::queue::tests::Driver) {
        use crate::irq::TestLine;
        use crate::virtio::queue::tests::{Driver, RAM_BASE};
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let mut ctl = Driver::new(8);
        let mut txd = Driver::new(8);
        for a in [&mut txd.cfg.desc, &mut txd.cfg.avail, &mut txd.cfg.used] {
            *a += 0x4000;
        }
        let mut w = Worker::new(
            mem,
            Arc::new(Interrupt::new(Arc::new(TestLine::default()))),
            Queue::new(ctl.cfg, false),
            [Some(Queue::new(txd.cfg, false)), None],
            opts,
            host,
        );
        w.tap = tap;
        for (i, r) in [set_params(0, 3840, 1920, 2, 5, 7), req(&[R_PCM_PREPARE, 0]), req(&[R_PCM_START, 0])].into_iter().enumerate() {
            let at = RAM_BASE + 0x8_0000 + i as u64 * 0x1000;
            w.mem.write(at, &r).expect("request");
            ctl.push(&w.mem, &[(at, r.len() as u32, false), (at + 0x800, 4, true)]);
            w.run();
            assert_eq!(w.mem.read_u32(at + 0x800).expect("status"), S_OK);
        }
        (w, txd)
    }

    fn play_bytes(w: &mut Worker, txd: &mut crate::virtio::queue::tests::Driver, at: u64, pcm: &[u8]) {
        w.mem.write(at, &0u32.to_le_bytes()).expect("xfer header");
        w.mem.write(at + 0x10, pcm).expect("pcm");
        txd.push(&w.mem, &[(at, 4, false), (at + 0x10, pcm.len() as u32, false), (at + 0x800, 8, true)]);
        w.run();
    }

    #[test]
    fn the_tap_copies_playback_under_no_audio_and_beside_the_host_output() {
        use crate::audio::tap::{Container, Sink as TapSink};
        use crate::virtio::queue::tests::RAM_BASE;
        let at = |i: u64| RAM_BASE + 0x9_0000 + i * 0x1000;
        let pcm: Vec<u8> = (0..=255u8).cycle().take(1920).collect();
        let captured = |tap: &Tap| {
            #[derive(Clone, Default)]
            struct Out(Arc<std::sync::Mutex<Vec<u8>>>);
            impl std::io::Write for Out {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().expect("out").extend_from_slice(b);
                    Ok(b.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let out = Out::default();
            tap.start(TapSink::Stream(Box::new(out.clone())), Container::Raw).expect("start");
            out
        };

        // `-no-audio`: no host, no output; the tap still gets the guest's bytes, exactly.
        let tap = Arc::new(Tap::default());
        let (mut w, mut txd) = playback_worker(Options::default(), None, tap.clone());
        play_bytes(&mut w, &mut txd, at(0), &pcm); // before a capture: nothing is kept
        let out = captured(&tap);
        play_bytes(&mut w, &mut txd, at(1), &pcm);
        let status = tap.stop().expect("running");
        assert_eq!(*out.0.lock().expect("out"), pcm);
        assert_eq!((status.captured_frames, status.dropped_frames), (480, 0));
        assert_eq!(status.format.map(|f| (f.rate, f.channels, f.format)), Some((48_000, 2, Format::S16)));
        assert!(w.links[0].ring.is_none(), "no host ring is filled without a host output");

        // With the host output on: the ring gets the same audio as before, and the tap a copy.
        let host = FakeHost::default();
        let opts = Options { output: true, input: false, verbose: false };
        let tap = Arc::new(Tap::default());
        let (mut w, mut txd) = playback_worker(opts, Some(Box::new(host.clone())), tap.clone());
        let ring = host.take().into_iter().find_map(|(_, r)| r).expect("opened on START");
        let out = captured(&tap);
        play_bytes(&mut w, &mut txd, at(0), &pcm);
        tap.stop();
        assert_eq!(*out.0.lock().expect("out"), pcm);
        assert_eq!(ring.len(), 480);
    }

    #[test]
    fn without_a_tap_the_host_ring_gets_exactly_the_guests_frames() {
        use crate::virtio::queue::tests::RAM_BASE;
        let host = FakeHost::default();
        let opts = Options { output: true, input: false, verbose: false };
        let (mut w, mut txd) = playback_worker(opts, Some(Box::new(host.clone())), Arc::default());
        let ring = host.take().into_iter().find_map(|(_, r)| r).expect("opened on START");
        let pcm: Vec<u8> = (0..480).flat_map(|_| [0x00, 0x40, 0x00, 0xc0]).collect();
        play_bytes(&mut w, &mut txd, RAM_BASE + 0x9_0000, &pcm);
        assert_eq!((ring.len(), ring.pop()), (480, Some([0.5, -0.5])));
        assert!(w.tap.feed_is_idle(), "no capture, no queue");
    }

    #[test]
    fn plays_and_records_through_the_host() {
        use crate::irq::TestLine;
        use crate::virtio::queue::tests::{Driver, RAM_BASE};
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let driver = |n: u64| {
            let mut d = Driver::new(8);
            for a in [&mut d.cfg.desc, &mut d.cfg.avail, &mut d.cfg.used] {
                *a += n * 0x4000;
            }
            d
        };
        let (mut ctl, mut txd, mut rxd) = (driver(0), driver(1), driver(2));
        let host = FakeHost::default();
        let opts = Options { output: true, input: true, verbose: false };
        let mut w = Worker::new(
            mem.clone(),
            Arc::new(Interrupt::new(line)),
            Queue::new(ctl.cfg, false),
            [Some(Queue::new(txd.cfg, false)), Some(Queue::new(rxd.cfg, false))],
            opts,
            Some(Box::new(host.clone())),
        );
        fn buf(i: u64) -> u64 {
            RAM_BASE + 0x8_0000 + i * 0x1000
        }
        let mut n = 0;
        let mut control = |w: &mut Worker, r: Vec<u8>| {
            w.mem.write(buf(n), &r).expect("request");
            ctl.push(&w.mem, &[(buf(n), r.len() as u32, false), (buf(n) + 0x800, 4, true)]);
            w.run();
            let status = w.mem.read_u32(buf(n) + 0x800).expect("status");
            n += 1;
            status
        };

        // Playback, 48 kHz stereo S16: a prefilled buffer is in the ring the host opens on START.
        assert_eq!(control(&mut w, set_params(0, 3840, 1920, 2, 5, 7)), S_OK);
        assert_eq!(control(&mut w, req(&[R_PCM_PREPARE, 0])), S_OK);
        assert_eq!(host.names(), ["close Output", "close Output"], "new parameters: nothing left open");
        let pcm: Vec<u8> = (0..480).flat_map(|_| [0x00, 0x40, 0x00, 0xc0]).collect(); // +0.5, -0.5
        mem.write(buf(20), &0u32.to_le_bytes()).expect("xfer header");
        mem.write(buf(21), &pcm).expect("pcm");
        txd.push(&mem, &[(buf(20), 4, false), (buf(21), 1920, false), (buf(20) + 0x800, 8, true)]);
        w.run();
        assert_eq!(control(&mut w, req(&[R_PCM_START, 0])), S_OK);
        let opened = host.take();
        assert_eq!(opened.len(), 1);
        let ring = opened[0].1.clone().expect("ring");
        assert_eq!((opened[0].0.as_str(), ring.len(), ring.rate()), ("open Output", 480, 48_000));
        assert_eq!(ring.pop(), Some([0.5, -0.5]));
        ring.push_all(&[[0.25; 2]; 4]);
        assert_eq!(control(&mut w, req(&[R_PCM_STOP, 0])), S_OK);
        assert_eq!(host.names(), ["close Output"], "no host stream while the guest's is stopped");
        assert!(w.links[0].ring.is_none(), "STOP drops what was left, so the next START doesn't play it late");

        // Capture, 48 kHz stereo S16: what the host pushes comes back in the guest's format.
        assert_eq!(control(&mut w, set_params(1, 3840, 960, 2, 5, 7)), S_OK);
        assert_eq!(control(&mut w, req(&[R_PCM_PREPARE, 1])), S_OK);
        assert_eq!(control(&mut w, req(&[R_PCM_START, 1])), S_OK);
        let opened = host.take().into_iter().find(|(n, _)| n == "open Input").expect("input opened");
        let ring = opened.1.expect("ring");
        ring.set_rate(48_000);
        ring.push_all(&vec![[0.5, -0.25]; 2000]);
        mem.write(buf(30), &1u32.to_le_bytes()).expect("xfer header");
        rxd.push(&mem, &[(buf(30), 4, false), (buf(31), 960, true), (buf(30) + 0x800, 8, true)]);
        w.run();
        std::thread::sleep(Duration::from_millis(6));
        w.run();
        assert_eq!((rxd.used(&mem, 0).1, mem.read_u32(buf(30) + 0x800).expect("status")), (968, S_OK));
        let mut got = [0u8; 8];
        mem.read(buf(31) + 400, &mut got).expect("read");
        assert_eq!(got, [0x00, 0x40, 0x00, 0xe0, 0x00, 0x40, 0x00, 0xe0], "+0.5, -0.25");
        assert_eq!(control(&mut w, req(&[R_PCM_STOP, 1])), S_OK);
        assert_eq!(control(&mut w, req(&[R_PCM_RELEASE, 1])), S_OK);
        assert_eq!(host.names(), ["close Input", "close Input"]);
    }
}
