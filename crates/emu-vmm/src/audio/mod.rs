//! Host audio for the virtio-snd card: guest PCM to and from stereo `f32` frames, rate
//! conversion, and the lock-free rings between the card's worker and the host's audio callbacks.
//!
//! Playback: the worker decodes each buffer the guest queues into the stream's [`Ring`] at the
//! guest's rate; the host output callback ([`host`]) pulls from it through a [`Reader`] at the
//! device's rate. Capture runs the other way: the host input callback pushes frames at the
//! device's rate and the worker reads them at the guest's rate when it completes a buffer.
//!
//! The card keeps the guest's clock (buffers complete at the stream's nominal rate, see
//! `virtio::snd`), so the host stream never paces the guest: an underrun plays silence, an overrun
//! drops frames, and a reader that falls too far behind skips ahead to bound the latency.
//!
//! Everything here but [`host`] is plain logic and tests on any host.
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub mod host;
pub mod tap;

/// What the card may do on the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Play guest output on the host's default output device (off with `-no-audio`).
    pub output: bool,
    /// Record guest input from the host's default input device (`-allow-host-audio`); otherwise
    /// capture is silence, as stock's.
    pub input: bool,
    /// Log host streams opening and closing, with their frame counts (`-verbose`).
    pub verbose: bool,
}

/// A guest sample format (`VIRTIO_SND_PCM_FMT_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    S16,
    /// 24 bits in the low three bytes of a 32-bit little-endian word (ALSA `S24_LE`).
    S24,
    S32,
    F32,
}

impl Format {
    /// The `VIRTIO_SND_PCM_FMT_*` code's format.
    pub fn from_virtio(code: u8) -> Option<Format> {
        match code {
            5 => Some(Format::S16),
            15 => Some(Format::S24),
            17 => Some(Format::S32),
            19 => Some(Format::F32),
            _ => None,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Format::S16 => 2,
            Format::S24 | Format::S32 | Format::F32 => 4,
        }
    }

    /// The name the capture status and console use: `s24` is 24 bits in a 32-bit word.
    pub fn name(self) -> &'static str {
        match self {
            Format::S16 => "s16",
            Format::S24 => "s24",
            Format::S32 => "s32",
            Format::F32 => "f32",
        }
    }

    fn decode(self, b: &[u8]) -> f32 {
        match self {
            Format::S16 => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
            // Sign-extend the low 24 bits.
            Format::S24 => ((i32::from_le_bytes([b[0], b[1], b[2], b[3]]) << 8) >> 8) as f32 / 8_388_608.0,
            Format::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            Format::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    }

    fn encode(self, v: f32, out: &mut Vec<u8>) {
        let v = if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) };
        match self {
            Format::S16 => out.extend_from_slice(&((v * 32_767.0).round() as i16).to_le_bytes()),
            Format::S24 => out.extend_from_slice(&((v * 8_388_607.0).round() as i32).to_le_bytes()),
            Format::S32 => out.extend_from_slice(&((f64::from(v) * 2_147_483_647.0).round() as i32).to_le_bytes()),
            Format::F32 => out.extend_from_slice(&v.to_le_bytes()),
        }
    }
}

/// One stereo frame: left, right.
pub type Frame = [f32; 2];

/// Decodes interleaved guest PCM (`channels` 1 or 2) into stereo frames appended to `out`; mono
/// goes to both sides. A trailing partial frame is ignored.
pub fn decode(format: Format, channels: usize, data: &[u8], out: &mut Vec<Frame>) {
    let size = format.bytes();
    let channels = channels.max(1);
    for f in data.chunks_exact(size * channels) {
        let l = format.decode(&f[..size]);
        let r = if channels > 1 { format.decode(&f[size..2 * size]) } else { l };
        out.push([l, r]);
    }
}

/// Encodes stereo frames as interleaved guest PCM (`channels` 1 or 2; mono is the mean),
/// appended to `out`.
pub fn encode(format: Format, channels: usize, frames: &[Frame], out: &mut Vec<u8>) {
    for &[l, r] in frames {
        if channels > 1 {
            format.encode(l, out);
            format.encode(r, out);
        } else {
            format.encode((l + r) / 2.0, out);
        }
    }
}

/// Writes a stereo frame into one interleaved host frame of `out.len()` channels: left and right
/// in the first two, silence in the rest, the mean on a mono device.
pub fn spread(frame: Frame, out: &mut [f32]) {
    match out {
        [] => {}
        [m] => *m = (frame[0] + frame[1]) / 2.0,
        [l, r, rest @ ..] => {
            *l = frame[0];
            *r = frame[1];
            rest.fill(0.0);
        }
    }
}

/// The stereo frame of one interleaved host frame: its first two channels, or a mono one twice.
pub fn gather(input: &[f32]) -> Frame {
    match input {
        [] => [0.0; 2],
        [m] => [*m; 2],
        [l, r, ..] => [*l, *r],
    }
}

/// Counters a ring keeps for `-verbose`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames pushed.
    pub pushed: u64,
    /// Of those, frames with a sample above -80 dB: audio rather than silence.
    pub audible: u64,
    /// Frames the producer dropped because the ring was full.
    pub dropped: u64,
    /// Frames the reader skipped to bound its latency.
    pub skipped: u64,
    /// Frames the reader filled with silence because the ring was empty.
    pub starved: u64,
}

/// A single-producer, single-consumer ring of stereo frames. Each frame is one `AtomicU64` (the
/// two samples' bits), so neither side ever blocks or allocates: the consumer may be a host audio
/// callback.
#[derive(Debug)]
pub struct Ring {
    slots: Box<[AtomicU64]>,
    /// Next slot to write (producer), and next to read (consumer); both count up forever.
    head: AtomicUsize,
    tail: AtomicUsize,
    /// The rate of the frames pushed, in Hz (0 until the producer sets it).
    rate: AtomicU32,
    pushed: AtomicU64,
    audible: AtomicU64,
    dropped: AtomicU64,
    skipped: AtomicU64,
    starved: AtomicU64,
}

const AUDIBLE: f32 = 1e-4;

impl Ring {
    /// A ring holding up to `capacity` frames (rounded up to a power of two), of audio at `rate` Hz.
    pub fn new(capacity: usize, rate: u32) -> Ring {
        let n = capacity.max(2).next_power_of_two();
        Ring {
            slots: (0..n).map(|_| AtomicU64::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            rate: AtomicU32::new(rate),
            pushed: AtomicU64::new(0),
            audible: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            starved: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Producer: the rate of the frames it pushes from now on.
    pub fn set_rate(&self, hz: u32) {
        self.rate.store(hz, Ordering::Relaxed);
    }

    /// Frames waiting to be read.
    pub fn len(&self) -> usize {
        self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Producer: appends `frame`; false (and counted as dropped) when the ring is full.
    pub fn push(&self, frame: Frame) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.tail.load(Ordering::Acquire)) >= self.slots.len() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let bits = u64::from(frame[0].to_bits()) | u64::from(frame[1].to_bits()) << 32;
        self.slots[head & (self.slots.len() - 1)].store(bits, Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        self.pushed.fetch_add(1, Ordering::Relaxed);
        if frame[0].abs() > AUDIBLE || frame[1].abs() > AUDIBLE {
            self.audible.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// Producer: appends what fits of `frames`; returns how many did.
    pub fn push_all(&self, frames: &[Frame]) -> usize {
        frames.iter().take_while(|f| self.push(**f)).count()
    }

    /// Consumer: the oldest frame.
    pub fn pop(&self) -> Option<Frame> {
        let tail = self.tail.load(Ordering::Relaxed);
        if self.head.load(Ordering::Acquire) == tail {
            return None;
        }
        let bits = self.slots[tail & (self.slots.len() - 1)].load(Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some([f32::from_bits(bits as u32), f32::from_bits((bits >> 32) as u32)])
    }

    /// Consumer: drops the oldest `n` frames (at most what's there).
    pub fn skip(&self, n: usize) {
        let n = n.min(self.len());
        self.tail.fetch_add(n, Ordering::Release);
        self.skipped.fetch_add(n as u64, Ordering::Relaxed);
    }

    fn starve(&self, n: usize) {
        self.starved.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn stats(&self) -> Stats {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Stats {
            pushed: get(&self.pushed),
            audible: get(&self.audible),
            dropped: get(&self.dropped),
            skipped: get(&self.skipped),
            starved: get(&self.starved),
        }
    }
}

/// Linear-interpolating rate converter over a stream of stereo frames.
#[derive(Clone, Debug)]
pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Position between `prev` (0) and `next` (1).
    t: f64,
    prev: Frame,
    next: Frame,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Resampler {
        // t = 2: the first output pulls two input frames and is exactly the first one.
        Resampler { step: f64::from(from_hz.max(1)) / f64::from(to_hz.max(1)), t: 2.0, prev: [0.0; 2], next: [0.0; 2] }
    }

    /// The next output frame, pulling input frames as needed; None (with nothing consumed past
    /// what was pulled) when `pull` runs dry.
    pub fn next(&mut self, mut pull: impl FnMut() -> Option<Frame>) -> Option<Frame> {
        while self.t >= 1.0 {
            let n = pull()?;
            self.prev = self.next;
            self.next = n;
            self.t -= 1.0;
        }
        let t = self.t as f32;
        let out = [self.prev[0] + (self.next[0] - self.prev[0]) * t, self.prev[1] + (self.next[1] - self.prev[1]) * t];
        self.t += self.step;
        Some(out)
    }
}

/// The most frames a [`Reader`] lets wait before it skips back: 200 ms at `hz`, or the guest's
/// whole buffer (`buffer_frames`) and the prime when that is more, so a guest that keeps a large
/// buffer queued isn't cut.
pub fn max_latency(hz: u32, buffer_frames: usize, prime: usize) -> usize {
    (hz as usize / 5).max(buffer_frames.saturating_add(prime))
}

/// The consumer side of a [`Ring`]: converts its frames to the reader's rate, waits for `prime`
/// frames before starting (and again after running dry), and skips back to `prime` frames of
/// latency when more than `max` are waiting.
#[derive(Debug)]
pub struct Reader {
    to_hz: u32,
    from_hz: u32,
    rs: Resampler,
    prime: usize,
    max: usize,
    playing: bool,
}

impl Reader {
    /// Reads at `to_hz`; `prime` and `max` are in the ring's frames.
    pub fn new(to_hz: u32, prime: usize, max: usize) -> Reader {
        Reader { to_hz, from_hz: 0, rs: Resampler::new(1, 1), prime: prime.max(1), max: max.max(prime + 1), playing: false }
    }

    /// Fills `out` with frames from `ring` (one call per host callback or guest buffer); returns
    /// how many came from it, the rest being silence.
    pub fn read(&mut self, ring: &Ring, out: &mut [Frame]) -> usize {
        let hz = ring.rate();
        if hz != self.from_hz {
            self.from_hz = hz;
            self.rs = Resampler::new(hz, self.to_hz);
        }
        let waiting = ring.len();
        if !self.playing && waiting >= self.prime && hz != 0 {
            self.playing = true;
        }
        if self.playing && waiting > self.max {
            ring.skip(waiting - self.prime);
        }
        let mut n = 0;
        if self.playing {
            while n < out.len() {
                match self.rs.next(|| ring.pop()) {
                    Some(f) => out[n] = f,
                    None => {
                        self.playing = false;
                        break;
                    }
                }
                n += 1;
            }
        }
        out[n..].fill([0.0; 2]);
        ring.starve(out.len() - n);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_encodes_every_format() {
        let mut frames = Vec::new();
        // S16 stereo: full scale negative, half positive.
        decode(Format::S16, 2, &[0x00, 0x80, 0x00, 0x40, 0xff], &mut frames);
        assert_eq!(frames, vec![[-1.0, 0.5]], "the trailing partial frame is ignored");
        frames.clear();
        // S24 in 32: -1 (0xffffff sign-extends), and 0x400000 = 0.5; the top byte is padding.
        decode(Format::S24, 2, &[0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x40, 0x7f], &mut frames);
        assert_eq!(frames, vec![[-1.0 / 8_388_608.0, 0.5]]);
        frames.clear();
        decode(Format::S32, 1, &i32::MIN.to_le_bytes(), &mut frames);
        decode(Format::F32, 1, &0.25f32.to_le_bytes(), &mut frames);
        assert_eq!(frames, vec![[-1.0, -1.0], [0.25, 0.25]], "mono goes to both sides");

        for (format, channels) in [(Format::S16, 2), (Format::S24, 1), (Format::S32, 2), (Format::F32, 1)] {
            let mut bytes = Vec::new();
            encode(format, channels, &[[0.5, 0.5], [-0.25, -0.25], [2.0, 2.0]], &mut bytes);
            assert_eq!(bytes.len(), 3 * channels * format.bytes());
            let mut back = Vec::new();
            decode(format, channels, &bytes, &mut back);
            for (got, want) in back.iter().zip([0.5, -0.25, 1.0]) {
                assert!((got[0] - want).abs() < 1e-4 && got[0] == got[1], "{format:?} {channels}: {got:?} vs {want}");
            }
        }
        let mut bytes = Vec::new();
        encode(Format::S16, 1, &[[1.0, 0.0], [f32::NAN, f32::NAN]], &mut bytes);
        assert_eq!(bytes, [0x00, 0x40, 0, 0], "mono is the mean; NaN is silence");
        assert_eq!(Format::from_virtio(5), Some(Format::S16));
        assert_eq!(Format::from_virtio(4), None, "U8 isn't offered");
    }

    #[test]
    fn spreads_and_gathers_host_channels() {
        let mut six = [9.0; 6];
        spread([0.5, -0.5], &mut six);
        assert_eq!(six, [0.5, -0.5, 0.0, 0.0, 0.0, 0.0]);
        let mut mono = [9.0];
        spread([0.5, 0.25], &mut mono);
        assert_eq!(mono, [0.375]);
        assert_eq!((gather(&[0.1, 0.2, 0.3]), gather(&[0.4]), gather(&[])), ([0.1, 0.2], [0.4, 0.4], [0.0, 0.0]));
    }

    #[test]
    fn ring_is_fifo_bounded_and_counts() {
        let ring = Ring::new(3, 48_000);
        assert_eq!((ring.capacity(), ring.rate()), (4, 48_000), "rounded up to a power of two");
        assert_eq!(ring.push_all(&[[0.0; 2], [0.5; 2], [1.0; 2], [-1.0; 2], [0.25; 2]]), 4);
        assert_eq!(ring.len(), 4);
        assert_eq!((ring.pop(), ring.pop()), (Some([0.0; 2]), Some([0.5; 2])));
        // Wraps around.
        assert!(ring.push([0.75, -0.75]));
        ring.skip(1);
        assert_eq!((ring.pop(), ring.pop(), ring.pop()), (Some([-1.0; 2]), Some([0.75, -0.75]), None));
        assert!(ring.is_empty());
        let s = ring.stats();
        assert_eq!((s.pushed, s.audible, s.dropped, s.skipped), (5, 4, 1, 1));
    }

    #[test]
    fn ring_carries_frames_between_threads() {
        let ring = std::sync::Arc::new(Ring::new(64, 1));
        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                for i in 0..10_000u32 {
                    while !ring.push([i as f32, -(i as f32)]) {
                        std::thread::yield_now();
                    }
                }
            })
        };
        let mut want = 0u32;
        while want < 10_000 {
            match ring.pop() {
                Some(f) => {
                    assert_eq!(f, [want as f32, -(want as f32)]);
                    want += 1;
                }
                None => std::thread::yield_now(),
            }
        }
        producer.join().expect("producer");
    }

    #[test]
    fn resamples_linearly() {
        let ramp = |n: usize| (0..n).map(|i| [i as f32, 0.0]).collect::<Vec<_>>();
        let run = |from: u32, to: u32, input: Vec<Frame>| {
            let mut it = input.into_iter();
            let mut rs = Resampler::new(from, to);
            std::iter::from_fn(|| rs.next(|| it.next())).map(|f| f[0]).collect::<Vec<_>>()
        };
        assert_eq!(run(48_000, 48_000, ramp(4)), [0.0, 1.0, 2.0], "same rate: input, less the last frame held back");
        assert_eq!(run(24_000, 48_000, ramp(3)), [0.0, 0.5, 1.0, 1.5], "doubling interpolates");
        assert_eq!(run(48_000, 24_000, ramp(7)), [0.0, 2.0, 4.0], "halving takes every other frame");
        // 44.1 kHz to 48 kHz: 441 frames in, about 480 out, monotone.
        let out = run(44_100, 48_000, ramp(441));
        assert!((478..=481).contains(&out.len()), "{}", out.len());
        assert!(out.windows(2).all(|w| w[1] > w[0]));

        // Running dry consumes nothing: the next call resumes where it stopped.
        let mut rs = Resampler::new(1, 1);
        assert_eq!(rs.next(|| None), None);
        let mut src = vec![[1.0, 1.0], [2.0, 2.0]].into_iter();
        assert_eq!(rs.next(|| src.next()), Some([1.0, 1.0]));
    }

    #[test]
    fn reader_primes_underruns_and_bounds_latency() {
        let ring = Ring::new(1024, 48_000);
        let mut r = Reader::new(48_000, 4, 16);
        let mut out = [[9.0; 2]; 3];
        ring.push_all(&[[0.5; 2]; 3]);
        assert_eq!(r.read(&ring, &mut out), 0, "not primed yet");
        assert_eq!(out, [[0.0; 2]; 3]);
        ring.push_all(&[[0.5; 2]; 3]);
        assert_eq!(r.read(&ring, &mut out), 3);
        assert_eq!(out, [[0.5; 2]; 3]);
        // Two left, one held back by the resampler: it runs dry after one more and pads.
        assert_eq!(r.read(&ring, &mut out), 2);
        assert_eq!(out[2], [0.0; 2]);
        assert_eq!(r.read(&ring, &mut out), 0, "waits to prime again");
        assert_eq!(ring.stats().starved, 1 + 3 + 3);

        // More than `max` waiting: skip back to `prime`.
        ring.push_all(&[[0.25; 2]; 40]);
        r.read(&ring, &mut out);
        assert_eq!(ring.stats().skipped, 36);
        assert!(ring.len() <= 4);

        // A ring with no rate yet (capture before the device opened) reads silence.
        let idle = Ring::new(16, 0);
        idle.push_all(&[[0.5; 2]; 8]);
        assert_eq!(Reader::new(48_000, 1, 8).read(&idle, &mut out), 0);
    }

    #[test]
    fn latency_bound_covers_the_guests_buffer() {
        assert_eq!(max_latency(48_000, 2176, 480), 9600, "a small buffer: 200 ms");
        assert_eq!(max_latency(48_000, 19_200, 480), 19_680, "a 400 ms buffer: all of it and the prime");
        assert_eq!(max_latency(48_000, usize::MAX, 480), usize::MAX);
        // A reader with that bound keeps a full 400 ms buffer instead of skipping it back.
        let ring = Ring::new(32_768, 48_000);
        ring.push_all(&vec![[0.5; 2]; 19_200]);
        Reader::new(48_000, 480, max_latency(48_000, 19_200, 480)).read(&ring, &mut [[0.0; 2]; 4]);
        assert_eq!(ring.stats().skipped, 0);
    }

    #[test]
    fn reader_converts_rates() {
        // 10 ms at 44.1 kHz read at 48 kHz: about 480 frames.
        let ring = Ring::new(4096, 44_100);
        ring.push_all(&vec![[0.5; 2]; 441]);
        let mut r = Reader::new(48_000, 1, 4096);
        let mut out = vec![[0.0; 2]; 600];
        let n = r.read(&ring, &mut out);
        assert!((478..=481).contains(&n), "{n}");
    }
}
