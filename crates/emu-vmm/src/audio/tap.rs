//! A host-side tap on the card's playback path: a second reader beside the host output that
//! copies the PCM the guest plays, exactly as the guest wrote it, to a file or a stream.
//!
//! The worker [`offer`](Tap::offer)s each playback buffer. With no capture running that is one
//! relaxed atomic load. With one running the bytes are copied into a bounded queue that a writer
//! thread drains, so a slow file or socket never stalls the guest: what doesn't fit is dropped and
//! counted in frames. The tap sits ahead of the host's ring, so it works under `-no-audio`.
//!
//! A capture holds what the guest plays, not wall-clock time: silence the guest doesn't queue is
//! not in it. A stream that changes format mid-capture has the frames in the new format dropped
//! (and counted), because one WAV file or raw stream has one format.
use std::borrow::Cow;
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::Format;

/// Bytes the queue between the card and the writer holds: about 5 s of 48 kHz stereo S16.
pub const DEFAULT_QUEUE_BYTES: usize = 1 << 20;

/// What the guest plays: the stream's rate, channels and sample format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u8,
    pub format: Format,
}

impl PcmFormat {
    pub fn frame_bytes(&self) -> usize {
        self.format.bytes() * usize::from(self.channels.max(1))
    }
}

impl std::fmt::Display for PcmFormat {
    /// `48000 Hz 2 ch s16`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} Hz {} ch {}", self.rate, self.channels, self.format.name())
    }
}

/// How a capture's bytes are framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// A RIFF/WAVE header (see [`wav_header`]) then the samples.
    Wav,
    /// The samples alone; the format is in [`Status::format`].
    Raw,
}

/// Where a capture goes. A seekable sink gets its WAV sizes patched when the capture ends; a
/// stream gets the "unknown length" sizes streaming WAV uses.
pub enum Sink {
    Seekable(Box<dyn WriteSeek>),
    Stream(Box<dyn Write + Send>),
}

pub trait WriteSeek: Write + Seek + Send {}
impl<T: Write + Seek + Send> WriteSeek for T {}

/// A capture's state, for the console's status reply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub capturing: bool,
    /// The guest has a sound card to tap (false: nothing will ever arrive).
    pub card: bool,
    /// The format of the frames captured; None until the guest has played something.
    pub format: Option<PcmFormat>,
    pub captured_frames: u64,
    /// Frames the queue couldn't take (a slow reader), or that came in another format.
    pub dropped_frames: u64,
    /// Why the writer stopped early (a closed socket, a full disk).
    pub error: Option<String>,
}

struct Chunk {
    format: PcmFormat,
    data: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    captured: AtomicU64,
    dropped: AtomicU64,
    /// Bytes queued and not yet written.
    queued: AtomicUsize,
    state: Mutex<Written>,
    /// Set by `stop`: when the writer must be done. Past it, what is still queued is dropped.
    deadline: Mutex<Option<Instant>>,
}

impl Shared {
    fn past_deadline(&self) -> bool {
        self.deadline.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|d| Instant::now() >= d)
    }
}

#[derive(Default)]
struct Written {
    format: Option<PcmFormat>,
    error: Option<String>,
}

struct Running {
    shared: Arc<Shared>,
    writer: JoinHandle<()>,
}

/// The tap: shared by the card (which offers buffers) and the console (which starts and stops).
pub struct Tap {
    active: AtomicBool,
    card: AtomicBool,
    capacity: usize,
    /// The queue's sending end and the capture's counters; locked by `offer` with `try_lock`.
    feed: Mutex<Option<(Sender<Chunk>, Arc<Shared>)>>,
    /// The running capture; control path only.
    running: Mutex<Option<Running>>,
}

impl Default for Tap {
    fn default() -> Self {
        Tap::new(DEFAULT_QUEUE_BYTES)
    }
}

impl Tap {
    /// A tap whose queue holds `capacity` bytes.
    pub fn new(capacity: usize) -> Tap {
        Tap { active: AtomicBool::new(false), card: AtomicBool::new(false), capacity, feed: Mutex::new(None), running: Mutex::new(None) }
    }

    /// The sound card calls this when it is attached.
    pub fn set_card(&self, present: bool) {
        self.card.store(present, Ordering::Relaxed);
    }

    /// No capture is running and no queue exists (for tests of the idle path).
    #[cfg(test)]
    pub(crate) fn feed_is_idle(&self) -> bool {
        !self.is_active() && self.feed.lock().is_ok_and(|f| f.is_none())
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Starts copying what the guest plays into `sink`. Err when one is already running.
    pub fn start(&self, sink: Sink, container: Container) -> Result<(), String> {
        self.start_with(|| Ok(sink), container)
    }

    /// Like [`start`](Tap::start), but `open` makes the sink only after the check that no capture
    /// is running, under the same lock: a refused start never creates or truncates a file.
    pub fn start_with(&self, open: impl FnOnce() -> Result<Sink, String>, container: Container) -> Result<(), String> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if running.is_some() {
            return Err("already capturing; stop it first".into());
        }
        let sink = open()?;
        let shared = Arc::new(Shared::default());
        let (tx, rx) = channel();
        let writer = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("snd-tap".into())
                .spawn(move || write_capture(rx, sink, container, &shared))
                .map_err(|e| e.to_string())?
        };
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = Some((tx, shared.clone()));
        self.active.store(true, Ordering::Release);
        *running = Some(Running { shared, writer });
        Ok(())
    }

    /// Ends the capture: what is queued is written and a WAV's sizes are finished, within
    /// [`STOP_BUDGET`] (a reader that has stalled gets the rest dropped and counted). The final
    /// status, or None when nothing was running.
    pub fn stop(&self) -> Option<Status> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let r = running.take()?;
        self.active.store(false, Ordering::Release);
        *r.shared.deadline.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + STOP_BUDGET);
        // Dropping the sender ends the writer once the queue is empty.
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let _ = r.writer.join();
        Some(self.report(false, &r.shared))
    }

    pub fn status(&self) -> Status {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        match running.as_ref() {
            Some(r) => self.report(true, &r.shared),
            None => Status { card: self.card.load(Ordering::Relaxed), ..Status::default() },
        }
    }

    fn report(&self, capturing: bool, shared: &Shared) -> Status {
        let st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        Status {
            capturing,
            card: self.card.load(Ordering::Relaxed),
            format: st.format,
            captured_frames: shared.captured.load(Ordering::Relaxed),
            dropped_frames: shared.dropped.load(Ordering::Relaxed),
            error: st.error.clone(),
        }
    }

    /// The card: one playback buffer of `data` (whole frames of `format`; a trailing partial frame
    /// is ignored). Never blocks: a full queue, a busy lock or a dead writer drops the buffer.
    pub fn offer(&self, format: PcmFormat, data: &[u8]) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        let frame = format.frame_bytes();
        let data = &data[..data.len() - data.len() % frame];
        let frames = (data.len() / frame) as u64;
        let Ok(feed) = self.feed.try_lock() else { return };
        let Some((tx, shared)) = feed.as_ref() else { return };
        if shared.queued.load(Ordering::Acquire) + data.len() > self.capacity {
            shared.dropped.fetch_add(frames, Ordering::Relaxed);
            return;
        }
        shared.queued.fetch_add(data.len(), Ordering::AcqRel);
        if tx.send(Chunk { format, data: data.to_vec() }).is_err() {
            // The writer ended early (its error is in the status).
            shared.queued.fetch_sub(data.len(), Ordering::AcqRel);
            shared.dropped.fetch_add(frames, Ordering::Relaxed);
        }
    }
}

/// The 44-byte RIFF/WAVE header (68 for 24-bit samples, which need `WAVE_FORMAT_EXTENSIBLE`) for
/// `data_len` bytes of samples; `u32::MAX` means unknown (a stream).
pub fn wav_header(f: PcmFormat, data_len: u32) -> Vec<u8> {
    let channels = u16::from(f.channels.max(1));
    let container_bits = (f.format.bytes() * 8) as u16;
    let block = channels * container_bits / 8;
    let extensible = f.format == Format::S24;
    let fmt_len: u32 = if extensible { 40 } else { 16 };
    let tag: u16 = match f.format {
        Format::F32 => 3,
        _ if extensible => 0xFFFE,
        _ => 1,
    };
    let riff = if data_len == u32::MAX { u32::MAX } else { data_len.saturating_add(4 + 8 + fmt_len + 8) };
    let mut h = Vec::with_capacity(68);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&riff.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&fmt_len.to_le_bytes());
    h.extend_from_slice(&tag.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&f.rate.to_le_bytes());
    h.extend_from_slice(&(f.rate * u32::from(block)).to_le_bytes());
    h.extend_from_slice(&block.to_le_bytes());
    h.extend_from_slice(&container_bits.to_le_bytes());
    if extensible {
        h.extend_from_slice(&22u16.to_le_bytes());
        h.extend_from_slice(&24u16.to_le_bytes());
        let mask: u32 = if channels == 1 { 0x4 } else { 0x3 };
        h.extend_from_slice(&mask.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM.
        h.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
    }
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    h
}

/// How often a WAV file's sizes are rewritten while capturing, so a capture that is never
/// finalized (a kill, a crash) still opens with at most this much audio missing.
const PATCH_EVERY: Duration = Duration::from_secs(1);

/// How long `stop` lets the writer drain what is queued before it drops the rest. One overall
/// bound, whatever the reader does; a write already in flight adds at most the socket's timeout.
pub const STOP_BUDGET: Duration = Duration::from_secs(2);

/// How many times in a row a stream may stall in the middle of a frame before the capture gives up.
const MID_FRAME_STALLS: u32 = 3;

fn is_stall(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Writes `bytes` to a stream whose writes may time out, and returns how many were written. A
/// timeout at a `unit` boundary (a whole frame) ends the write there: the rest is the caller's to
/// drop. A timeout inside a unit retries, because half a frame would shift every sample after it;
/// `None` is one unit that must finish (a header). Retries stop after `MID_FRAME_STALLS`, or once
/// `give_up` says the capture's stop deadline has passed. Only a real error (a closed peer, or
/// those) is an Err.
fn write_stream(w: &mut dyn Write, bytes: &[u8], unit: Option<usize>, give_up: &dyn Fn() -> bool) -> std::io::Result<usize> {
    let (mut done, mut stalls) = (0, 0);
    while done < bytes.len() {
        match w.write(&bytes[done..]) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => (done, stalls) = (done + n, 0),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if is_stall(&e) => {
                if unit.is_some_and(|u| done % u == 0) {
                    break;
                }
                stalls += 1;
                if stalls >= MID_FRAME_STALLS || give_up() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Samples as a WAV holds them. S24 is 24 bits in the low bytes of a 32-bit word in the guest's
/// stream; the header declares 24 valid bits in 32, which WAVE_FORMAT_EXTENSIBLE left-justifies,
/// so each word moves up a byte (the padding byte, which may hold anything, is dropped).
fn wav_samples(format: Format, data: &[u8]) -> Cow<'_, [u8]> {
    if format != Format::S24 {
        return Cow::Borrowed(data);
    }
    Cow::Owned(data.as_chunks::<4>().0.iter().flat_map(|w| (u32::from_le_bytes(*w) << 8).to_le_bytes()).collect())
}

/// Rewrites a WAV file's header with the sizes so far and returns to the end.
fn patch_sizes(w: &mut dyn WriteSeek, f: PcmFormat, data_bytes: u64) -> std::io::Result<()> {
    w.seek(SeekFrom::Start(0))?;
    w.write_all(&wav_header(f, u32::try_from(data_bytes).unwrap_or(u32::MAX - 1)))?;
    w.seek(SeekFrom::End(0))?;
    w.flush()
}

/// The writer thread: drains the queue into `sink` until the sender is dropped.
fn write_capture(rx: Receiver<Chunk>, mut sink: Sink, container: Container, shared: &Shared) {
    let mut format: Option<PcmFormat> = None;
    let mut data_bytes: u64 = 0;
    let mut patched = (Instant::now(), 0u64);
    let fail = |e: std::io::Error| shared.state.lock().unwrap_or_else(|p| p.into_inner()).error = Some(e.to_string());
    loop {
        let chunk = match rx.recv_timeout(PATCH_EVERY) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => {
                if let (Sink::Seekable(w), Some(f), Container::Wav) = (&mut sink, format, container)
                    && patched.1 != data_bytes
                {
                    let _ = patch_sizes(w.as_mut(), f, data_bytes);
                    patched = (Instant::now(), data_bytes);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let len = chunk.data.len();
        let frame = chunk.format.frame_bytes();
        let frames = (len / frame) as u64;
        if shared.past_deadline() || format.is_some_and(|f| f != chunk.format) {
            shared.dropped.fetch_add(frames, Ordering::Relaxed);
        } else {
            let mut res = Ok(());
            if format.is_none() {
                format = Some(chunk.format);
                shared.state.lock().unwrap_or_else(|p| p.into_inner()).format = format;
                if container == Container::Wav {
                    let header = wav_header(chunk.format, u32::MAX);
                    res = match &mut sink {
                        Sink::Seekable(w) => w.write_all(&header),
                        // The header is not droppable: a stall in it retries.
                        Sink::Stream(w) => write_stream(w.as_mut(), &header, None, &|| shared.past_deadline()).map(|_| ()),
                    };
                }
            }
            let samples =
                if container == Container::Wav { wav_samples(chunk.format.format, &chunk.data) } else { Cow::Borrowed(&chunk.data[..]) };
            let written = res.and_then(|()| match &mut sink {
                Sink::Seekable(w) => w.write_all(&samples).map(|()| samples.len()),
                Sink::Stream(w) => write_stream(w.as_mut(), &samples, Some(frame), &|| shared.past_deadline()),
            });
            match written {
                Ok(n) => {
                    data_bytes += n as u64;
                    let wrote = (n / frame) as u64;
                    shared.captured.fetch_add(wrote, Ordering::Relaxed);
                    // A stalled reader: what it didn't take is dropped and counted.
                    shared.dropped.fetch_add(frames - wrote, Ordering::Relaxed);
                }
                Err(e) => {
                    fail(e);
                    // Dropping the receiver makes every later offer count as dropped; what is
                    // already queued is counted here.
                    shared.dropped.fetch_add(frames, Ordering::Relaxed);
                    while let Ok(c) = rx.try_recv() {
                        shared.dropped.fetch_add((c.data.len() / c.format.frame_bytes()) as u64, Ordering::Relaxed);
                    }
                    return;
                }
            }
            if let (Sink::Seekable(w), Some(f), Container::Wav) = (&mut sink, format, container)
                && patched.0.elapsed() >= PATCH_EVERY
            {
                let _ = patch_sizes(w.as_mut(), f, data_bytes);
                patched = (Instant::now(), data_bytes);
            }
        }
        shared.queued.fetch_sub(len, Ordering::AcqRel);
    }
    // The capture ended. A WAV with nothing played still gets a header: the stock stream format.
    let f = format.unwrap_or(PcmFormat { rate: 48_000, channels: 2, format: Format::S16 });
    let res = match (&mut sink, container) {
        (Sink::Seekable(w), Container::Wav) => (|| {
            if format.is_none() {
                w.write_all(&wav_header(f, 0))?;
            }
            patch_sizes(w.as_mut(), f, data_bytes)
        })(),
        (Sink::Stream(w), Container::Wav) if format.is_none() => w.write_all(&wav_header(f, u32::MAX)).and_then(|()| w.flush()),
        (Sink::Seekable(w), _) => w.flush(),
        (Sink::Stream(w), _) => w.flush(),
    };
    if let Err(e) = res {
        fail(e);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const S16: PcmFormat = PcmFormat { rate: 48_000, channels: 2, format: Format::S16 };

    /// A sink the test can read back: a cursor behind a lock.
    #[derive(Clone, Default)]
    struct Mem(Arc<Mutex<Cursor<Vec<u8>>>>);

    impl Mem {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().expect("mem").get_ref().clone()
        }
    }

    impl Write for Mem {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("mem").write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Mem {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().expect("mem").seek(to)
        }
    }

    /// A stream sink that holds every write until the test lets go of `gate`.
    struct Gated(Mem, Arc<Mutex<()>>);

    impl Write for Gated {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let _wait = self.1.lock().expect("gate");
            self.0.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
    }

    #[test]
    fn a_raw_capture_is_exactly_the_bytes_played() {
        let tap = Tap::default();
        let out = Mem::default();
        assert!(!tap.status().capturing);
        tap.start(Sink::Seekable(Box::new(out.clone())), Container::Raw).expect("start");
        let a: Vec<u8> = (0..64).collect();
        let b: Vec<u8> = (64..128).collect();
        tap.offer(S16, &a);
        tap.offer(S16, &[&b[..], &[0xee, 0xee, 0xee]].concat()); // a trailing partial frame is ignored
        let status = tap.stop().expect("running");
        assert_eq!(out.bytes(), (0..128).collect::<Vec<u8>>());
        assert_eq!(
            status,
            Status { capturing: false, card: false, format: Some(S16), captured_frames: 32, dropped_frames: 0, error: None }
        );
    }

    #[test]
    fn nothing_is_copied_without_a_capture() {
        let tap = Tap::default();
        tap.offer(S16, &[1, 2, 3, 4]);
        assert!(tap.feed.lock().expect("feed").is_none(), "no queue exists");
        assert_eq!(tap.status(), Status::default());
        assert_eq!(tap.stop(), None);
        // Bytes played before a capture starts aren't in it.
        let out = Mem::default();
        tap.start(Sink::Stream(Box::new(out.clone())), Container::Raw).expect("start");
        tap.stop();
        assert!(out.bytes().is_empty());
    }

    #[test]
    fn a_slow_reader_drops_whole_frames_and_counts_them() {
        let tap = Tap::new(100);
        let out = Mem::default();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().expect("gate");
        tap.start(Sink::Stream(Box::new(Gated(out.clone(), gate.clone()))), Container::Raw).expect("start");
        let chunk = [7u8; 40]; // 10 frames
        for _ in 0..4 {
            tap.offer(S16, &chunk);
        }
        // 40 + 40 fit in 100 bytes; the other two are dropped, and the offers never waited.
        assert_eq!(tap.status().dropped_frames, 20);
        drop(held);
        let status = tap.stop().expect("running");
        assert_eq!((status.captured_frames, status.dropped_frames), (20, 20));
        assert_eq!(out.bytes(), [7u8; 80]);
    }

    #[test]
    fn start_and_stop_work_twice_in_a_row() {
        let tap = Tap::default();
        tap.set_card(true);
        for round in 0u8..2 {
            let out = Mem::default();
            tap.start(Sink::Stream(Box::new(out.clone())), Container::Raw).expect("start");
            assert!(tap.is_active() && tap.status().capturing);
            assert!(tap.start(Sink::Stream(Box::new(Mem::default())), Container::Raw).is_err(), "one at a time");
            tap.offer(S16, &[round; 8]);
            let status = tap.stop().expect("running");
            assert_eq!((status.captured_frames, status.card), (2, true), "counters restart each capture");
            assert_eq!(out.bytes(), [round; 8]);
            assert!(!tap.is_active() && tap.stop().is_none());
            tap.offer(S16, &[9; 8]);
        }
    }

    #[test]
    fn a_format_change_mid_capture_drops_the_new_frames() {
        let tap = Tap::default();
        let out = Mem::default();
        tap.start(Sink::Stream(Box::new(out.clone())), Container::Raw).expect("start");
        tap.offer(S16, &[1; 8]);
        tap.offer(PcmFormat { rate: 44_100, ..S16 }, &[2; 8]);
        let status = tap.stop().expect("running");
        assert_eq!((status.captured_frames, status.dropped_frames, status.format), (2, 2, Some(S16)));
        assert_eq!(out.bytes(), [1; 8]);
    }

    #[test]
    fn a_failing_sink_is_reported_and_never_blocks_the_guest() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let tap = Tap::default();
        tap.start(Sink::Stream(Box::new(Broken)), Container::Raw).expect("start");
        tap.offer(S16, &[0; 8]);
        let status = tap.stop().expect("running");
        assert_eq!(status.error.as_deref(), Some("closed"));
        assert_eq!((status.captured_frames, status.dropped_frames), (0, 2));
    }

    #[test]
    fn the_wav_header_describes_each_format() {
        let h = wav_header(S16, 1000);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[..4], b"RIFF");
        assert_eq!(u32_at(&h, 4), 1000 + 36);
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(&h, 16), 16);
        assert_eq!(u16::from_le_bytes([h[20], h[21]]), 1, "PCM");
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 2, "channels");
        assert_eq!(u32_at(&h, 24), 48_000);
        assert_eq!(u32_at(&h, 28), 192_000, "byte rate");
        assert_eq!(u16::from_le_bytes([h[32], h[33]]), 4, "block align");
        assert_eq!(u16::from_le_bytes([h[34], h[35]]), 16, "bits");
        assert_eq!((&h[36..40], u32_at(&h, 40)), (&b"data"[..], 1000));

        let f = wav_header(PcmFormat { rate: 16_000, channels: 1, format: Format::F32 }, 8);
        assert_eq!((u16::from_le_bytes([f[20], f[21]]), u16::from_le_bytes([f[34], f[35]])), (3, 32), "IEEE float");
        assert_eq!(u32_at(&f, 28), 64_000);

        let x = wav_header(PcmFormat { rate: 48_000, channels: 2, format: Format::S24 }, 8);
        assert_eq!((x.len(), u32_at(&x, 4), u32_at(&x, 16)), (68, 8 + 60, 40));
        assert_eq!(u16::from_le_bytes([x[20], x[21]]), 0xFFFE, "extensible");
        assert_eq!((u16::from_le_bytes([x[34], x[35]]), u16::from_le_bytes([x[38], x[39]])), (32, 24), "24 bits in 32");
        assert_eq!((u32_at(&x, 40), x[44]), (3, 1), "front left and right; the PCM subformat");
        assert_eq!(&x[60..64], b"data");

        assert_eq!(u32_at(&wav_header(S16, u32::MAX), 4), u32::MAX, "a stream's length is unknown");
    }

    #[test]
    fn a_wav_file_gets_its_sizes_on_stop_and_a_stream_the_unknown_ones() {
        let tap = Tap::default();
        let pcm: Vec<u8> = (0..16).collect();

        let file = Mem::default();
        tap.start(Sink::Seekable(Box::new(file.clone())), Container::Wav).expect("start");
        tap.offer(S16, &pcm);
        tap.stop();
        let want = [wav_header(S16, 16), pcm.clone()].concat();
        assert_eq!(file.bytes(), want);

        let stream = Mem::default();
        tap.start(Sink::Stream(Box::new(stream.clone())), Container::Wav).expect("start");
        tap.offer(S16, &pcm);
        tap.stop();
        assert_eq!(stream.bytes(), [wav_header(S16, u32::MAX), pcm].concat());

        // Nothing played: a valid, empty WAV in the stock format.
        let empty = Mem::default();
        tap.start(Sink::Seekable(Box::new(empty.clone())), Container::Wav).expect("start");
        tap.stop();
        assert_eq!(empty.bytes(), wav_header(S16, 0));
    }

    #[test]
    fn a_refused_start_never_opens_its_sink() {
        let tap = Tap::default();
        tap.start(Sink::Stream(Box::new(Mem::default())), Container::Raw).expect("start");
        let mut opened = false;
        let err = tap.start_with(
            || {
                opened = true;
                Ok(Sink::Stream(Box::new(Mem::default())))
            },
            Container::Raw,
        );
        assert!(err.is_err() && !opened, "the second start is refused before its file is created");
        tap.stop();
        // A sink that can't be opened is the caller's error and leaves the tap idle.
        assert_eq!(tap.start_with(|| Err("nope".into()), Container::Raw), Err("nope".into()));
        assert!(!tap.is_active());
    }

    #[test]
    fn s24_samples_are_left_justified_in_a_wav_and_untouched_raw() {
        let s24 = PcmFormat { rate: 48_000, channels: 1, format: Format::S24 };
        // 0x123456 and -2 (0xfffffe) in the low 24 bits; the first has garbage padding.
        let guest = [0x56, 0x34, 0x12, 0x7f, 0xfe, 0xff, 0xff, 0x00];
        let tap = Tap::default();
        let wav = Mem::default();
        tap.start(Sink::Seekable(Box::new(wav.clone())), Container::Wav).expect("start");
        tap.offer(s24, &guest);
        tap.stop();
        let bytes = wav.bytes();
        let data = &bytes[68..];
        assert_eq!(data, [0x00, 0x56, 0x34, 0x12, 0x00, 0xfe, 0xff, 0xff]);
        // Read as the header says (32-bit container, 24 valid bits, left-justified): the sample values.
        let values: Vec<i32> = data.chunks(4).map(|w| i32::from_le_bytes(w.try_into().expect("4")) >> 8).collect();
        assert_eq!(values, [0x123456, -2]);
        assert_eq!(u32_at(&bytes, 64), 8, "the data size is the shifted length");

        let raw = Mem::default();
        tap.start(Sink::Stream(Box::new(raw.clone())), Container::Raw).expect("start");
        tap.offer(s24, &guest);
        tap.stop();
        assert_eq!(raw.bytes(), guest, "a raw capture is what the guest wrote");
    }

    /// A stream whose writes time out for the first `stalls` calls, and take only `first` bytes of the
    /// very first one when set: a reader that paused.
    struct Stalling {
        out: Mem,
        stalls: usize,
        partial_first: Option<usize>,
    }

    impl Write for Stalling {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Some(n) = self.partial_first.take() {
                return self.out.write(&b[..n.min(b.len())]);
            }
            if self.stalls > 0 {
                self.stalls -= 1;
                return Err(ErrorKind::TimedOut.into());
            }
            self.out.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stalled_reader_drops_frames_and_the_capture_goes_on() {
        let tap = Tap::default();
        let out = Mem::default();
        tap.start(Sink::Stream(Box::new(Stalling { out: out.clone(), stalls: 1, partial_first: None })), Container::Raw).expect("start");
        tap.offer(S16, &[1; 8]);
        // Wait until the writer has handled the first buffer (it stalled and dropped it).
        while tap.status().dropped_frames == 0 {
            std::thread::yield_now();
        }
        tap.offer(S16, &[2; 8]);
        let status = tap.stop().expect("running");
        assert_eq!(status.error, None, "a timeout is not the end of the capture");
        assert_eq!((status.captured_frames, status.dropped_frames), (2, 2));
        assert_eq!(out.bytes(), [2; 8]);
    }

    #[test]
    fn a_stall_inside_a_frame_finishes_the_frame_so_the_stream_stays_aligned() {
        let tap = Tap::default();
        let out = Mem::default();
        // 6 bytes of 4-byte frames get through, then one timeout, then the reader resumes.
        let sink = Stalling { out: out.clone(), stalls: 1, partial_first: Some(6) };
        tap.start(Sink::Stream(Box::new(sink)), Container::Raw).expect("start");
        tap.offer(S16, &[3; 12]);
        let status = tap.stop().expect("running");
        assert_eq!(status.error, None);
        assert_eq!(out.bytes(), [3; 12], "the half frame was completed, not cut");
        assert_eq!((status.captured_frames, status.dropped_frames), (3, 0));
    }

    #[test]
    fn a_closed_peer_still_ends_the_capture() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let tap = Tap::default();
        tap.start(Sink::Stream(Box::new(Closed)), Container::Raw).expect("start");
        tap.offer(S16, &[0; 8]);
        let status = tap.stop().expect("running");
        assert!(status.error.is_some());
    }

    #[test]
    fn a_wav_file_never_finalized_still_has_its_sizes() {
        let tap = Tap::default();
        let file = Mem::default();
        tap.start(Sink::Seekable(Box::new(file.clone())), Container::Wav).expect("start");
        let pcm = [5u8; 16];
        tap.offer(S16, &pcm);
        // Past the patch interval, with the capture still running (as if emu were killed now).
        std::thread::sleep(PATCH_EVERY + Duration::from_millis(600));
        let live = file.bytes();
        assert_eq!(live, [wav_header(S16, 16), pcm.to_vec()].concat(), "readable without a stop");
        tap.stop();
        assert_eq!(file.bytes(), live);
    }

    #[test]
    fn stop_is_bounded_behind_a_reader_that_never_reads() {
        /// Every write times out after a pause, like a socket whose reader is suspended.
        struct Never;
        impl Write for Never {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(200));
                Err(ErrorKind::TimedOut.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let tap = Tap::default();
        tap.start(Sink::Stream(Box::new(Never)), Container::Raw).expect("start");
        for _ in 0..100 {
            tap.offer(S16, &[0; 8]); // 100 chunks: 20 s of timeouts if each had to be tried
        }
        let began = Instant::now();
        let status = tap.stop().expect("running");
        assert!(began.elapsed() < STOP_BUDGET + Duration::from_secs(2), "stop took {:?}", began.elapsed());
        assert_eq!((status.captured_frames, status.dropped_frames, status.error), (0, 200, None), "all counted");
    }

    #[test]
    fn a_wav_header_that_stalls_on_a_stream_is_retried_not_dropped() {
        let tap = Tap::default();
        let out = Mem::default();
        tap.start(Sink::Stream(Box::new(Stalling { out: out.clone(), stalls: 1, partial_first: None })), Container::Wav).expect("start");
        tap.offer(S16, &[4; 8]);
        let status = tap.stop().expect("running");
        assert_eq!(status.error, None);
        assert_eq!(out.bytes(), [wav_header(S16, u32::MAX), vec![4; 8]].concat());
    }

    #[test]
    fn a_failed_writer_counts_what_was_queued_behind_it() {
        struct Slow;
        impl Write for Slow {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(300));
                Err(ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let tap = Tap::default();
        tap.start(Sink::Stream(Box::new(Slow)), Container::Raw).expect("start");
        for _ in 0..5 {
            tap.offer(S16, &[0; 8]);
        }
        let status = tap.stop().expect("running");
        assert!(status.error.is_some());
        assert_eq!(status.dropped_frames, 10, "no frame goes uncounted");
    }
}
