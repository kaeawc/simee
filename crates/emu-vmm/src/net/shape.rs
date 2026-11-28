//! Network shaping for the NAT, as the console's `network speed|delay` asks for it (stock's
//! netshaper and netdelay): a token-bucket rate for each direction (0 = unlimited) and a
//! latency drawn between a minimum and a maximum for each frame inbound, and simee's
//! `network loss`: a percent of each direction's IP packets dropped at random. Pure logic over
//! caller-supplied instants and seeds, so it tests without sleeping or chance.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sim_core::network::Conditions;

use super::packet;

/// xorshift64*: one step of a nonzero state, its top 53 bits as a draw in [0, 1).
fn draw(state: &mut u64) -> f64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
}

/// splitmix64 of `x`: spreads a small seed over the state, so seeds 1 and 2 start far apart.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A seed for one boot's loss, from the clock.
pub fn clock_seed() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64)
}

/// One direction's loss: the percent dropped and its own random stream.
struct Lossy {
    pct: f64,
    rng: u64,
}

impl Lossy {
    fn drops(&mut self, frame: &[u8]) -> bool {
        // No draw for no loss or for a frame that is never dropped, so neither moves the pattern.
        self.pct > 0.0 && packet::droppable(frame) && draw(&mut self.rng) * 100.0 < self.pct
    }
}

/// `network loss`: each direction drops its percent of the guest's IP packets (TCP, UDP and
/// ICMP; never ARP) from a stream of its own, seeded once per boot: what one direction carries
/// never moves the other's pattern, and a fixed seed repeats both exactly.
pub struct Loss {
    up: Lossy,
    down: Lossy,
}

impl Loss {
    pub fn new(seed: u64) -> Loss {
        let stream = |salt: u64| mix(seed ^ salt) | 1;
        Loss { up: Lossy { pct: 0.0, rng: stream(0x7570) }, down: Lossy { pct: 0.0, rng: stream(0x646f_776e) } }
    }

    /// Sets each direction's percent (clamped to 0..=100; NaN is none). The streams go on.
    pub fn set(&mut self, up_pct: f64, down_pct: f64) {
        let pct = |p: f64| if p.is_nan() { 0.0 } else { p.clamp(0.0, 100.0) };
        (self.up.pct, self.down.pct) = (pct(up_pct), pct(down_pct));
    }

    /// Whether to drop a frame from the guest.
    pub fn drop_up(&mut self, frame: &[u8]) -> bool {
        self.up.drops(frame)
    }

    /// Whether to drop a frame for the guest.
    pub fn drop_down(&mut self, frame: &[u8]) -> bool {
        self.down.drops(frame)
    }
}

/// The smallest burst a bucket holds, in bits: one full-size frame, so a frame always fits.
const MIN_BURST_BITS: f64 = 12_144.0;
/// A bucket holds this much of its rate as burst (stock's shaper allows about as much).
const BURST: f64 = 0.02;

/// A token bucket of bits. A frame is always let through once the bucket is not in debt and
/// then charged in full, so the bucket may go negative by one frame: the average rate is exact
/// at any frame size.
pub struct Bucket {
    rate_bps: f64,
    tokens: f64,
    last: Option<Instant>,
}

impl Bucket {
    pub fn new() -> Bucket {
        Bucket { rate_bps: 0.0, tokens: 0.0, last: None }
    }

    fn capacity(&self) -> f64 {
        (self.rate_bps * BURST).max(MIN_BURST_BITS)
    }

    /// Changes the rate (0 = unlimited). Going from unlimited to a rate starts with a full
    /// bucket; a lower rate keeps what it can hold and any debt.
    pub fn set_rate(&mut self, bps: f64) {
        let bps = if bps.is_finite() && bps > 0.0 { bps } else { 0.0 };
        let was_unlimited = self.rate_bps == 0.0;
        self.rate_bps = bps;
        if bps == 0.0 {
            self.tokens = 0.0;
        } else if was_unlimited {
            self.tokens = self.capacity();
        } else {
            self.tokens = self.tokens.min(self.capacity());
        }
    }

    /// Starts over with a full bucket and no debt (the rate stays).
    pub fn reset(&mut self) {
        self.tokens = if self.rate_bps > 0.0 { self.capacity() } else { 0.0 };
        self.last = None;
    }

    fn refill(&mut self, now: Instant) {
        if let Some(last) = self.last.replace(now).filter(|_| self.rate_bps > 0.0) {
            let gained = now.saturating_duration_since(last).as_secs_f64() * self.rate_bps;
            self.tokens = (self.tokens + gained).min(self.capacity());
        }
    }

    /// How long until a frame may go: zero unless the bucket is in debt.
    pub fn wait(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.rate_bps == 0.0 || self.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-self.tokens / self.rate_bps) }
    }

    /// Charges a frame of `bytes`; returns how long until it may go (the debt it leaves).
    pub fn take(&mut self, now: Instant, bytes: usize) -> Duration {
        self.refill(now);
        if self.rate_bps == 0.0 {
            return Duration::ZERO;
        }
        self.tokens -= bytes as f64 * 8.0;
        if self.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-self.tokens / self.rate_bps) }
    }
}

/// The bits of a full-size frame, which `Downlink::room` counts in.
const FRAME_BITS: u64 = 1514 * 8;
/// A rate holds this long a backlog and refuses what comes past it, as a real bottleneck's
/// queue does: a sender that dumps a whole window must not park seconds of data in the link.
const QUEUE: f64 = 1.0;

/// How many bits a link at `bps` holds: [`QUEUE`] seconds of it, and never less than two
/// frames (so a frame always fits). `None` for an unlimited link.
pub fn queue_bits(bps: f64) -> Option<u64> {
    (bps > 0.0).then(|| ((bps * QUEUE) as u64).max(2 * FRAME_BITS))
}

/// Extra room, past the queue's cap, that control frames (ACKs, SYN-ACKs, ARP replies) may
/// use, in bits: bulk data finds the queue full while a reply to the guest's own frame still
/// gets through. It bounds the memory they can pin (about 24 kB however many arrive).
const CONTROL_RESERVE_BITS: u64 = 16 * FRAME_BITS;

/// A frame held for the guest: when it is due, when the bucket let it go (before the latency),
/// and when it arrived.
struct Held {
    due: Instant,
    sent: Instant,
    at: Instant,
    frame: Vec<u8>,
}

/// Frames for the guest, each held until its time: the download rate first, then a latency
/// between the minimum and maximum. Frames leave in the order they came in (a TCP flow
/// would read reordering as loss), so a short delay behind a long one waits for it. At a rate
/// the queue holds about a second of it; frames past that are refused.
pub struct Downlink {
    bucket: Bucket,
    min_ms: u32,
    max_ms: u32,
    rng: u64,
    last_due: Option<Instant>,
    queue: VecDeque<Held>,
    held_bits: u64,
}

impl Downlink {
    pub fn new() -> Downlink {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        Downlink { bucket: Bucket::new(), min_ms: 0, max_ms: 0, rng: seed | 1, last_due: None, queue: VecDeque::new(), held_bits: 0 }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Applies new conditions to the frames already held. A new rate times them again from
    /// when they arrived, as if it had always applied (lifting a throttle frees the backlog
    /// now); the same rate keeps each frame's turn, and a new latency alone only redraws the
    /// latency. Either way a frame is charged once.
    pub fn set(&mut self, c: &Conditions) {
        let rate = if c.download_bps.is_finite() && c.download_bps > 0.0 { c.download_bps } else { 0.0 };
        let retime = rate != self.bucket.rate_bps;
        self.bucket.set_rate(rate);
        // Stock stores the latencies as ints and a negative one is no delay.
        self.min_ms = c.min_latency_ms.max(0) as u32;
        self.max_ms = (c.max_latency_ms.max(0) as u32).max(self.min_ms);
        self.last_due = None;
        let held = std::mem::take(&mut self.queue);
        self.held_bits = 0;
        if retime {
            self.bucket.reset();
        }
        for h in held {
            if retime {
                self.enqueue(h.at, h.frame);
            } else {
                self.hold(h);
            }
        }
    }

    fn jitter(&mut self) -> Duration {
        if self.max_ms == self.min_ms {
            return Duration::from_millis(self.min_ms.into());
        }
        // xorshift64*: the spread only needs to look uniform.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let r = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        let span = u64::from(self.max_ms - self.min_ms) * 1000 + 1;
        Duration::from_micros(u64::from(self.min_ms) * 1000 + r % span)
    }

    /// Whether `bytes` more fit in the queue: always, with no rate.
    fn fits(&self, bytes: usize) -> bool {
        queue_bits(self.bucket.rate_bps).is_none_or(|cap| self.held_bits + bytes as u64 * 8 <= cap)
    }

    /// How many more full-size frames the queue takes (any number, with no rate).
    pub fn room(&self) -> usize {
        queue_bits(self.bucket.rate_bps).map_or(usize::MAX, |cap| (cap.saturating_sub(self.held_bits) / FRAME_BITS) as usize)
    }

    /// How many full-size frames the queue holds in all (any number, with no rate): a
    /// datagram of more fragments than this can never fit.
    pub fn capacity(&self) -> usize {
        queue_bits(self.bucket.rate_bps).map_or(usize::MAX, |cap| (cap / FRAME_BITS) as usize)
    }

    /// Charges a frame that arrived at `at` and queues it.
    fn enqueue(&mut self, at: Instant, frame: Vec<u8>) {
        let sent = at + self.bucket.take(at, frame.len());
        self.hold(Held { due: sent, sent, at, frame });
    }

    /// Queues a frame after the latency, behind the frame before it.
    fn hold(&mut self, mut h: Held) {
        h.due = h.sent + self.jitter();
        if let Some(last) = self.last_due {
            h.due = h.due.max(last);
        }
        self.last_due = Some(h.due);
        self.held_bits += h.frame.len() as u64 * 8;
        self.queue.push_back(h);
    }

    /// Holds a frame for the guest; false (the frame is dropped) when the queue is full.
    pub fn push(&mut self, now: Instant, frame: Vec<u8>) -> bool {
        if !self.fits(frame.len()) {
            return false;
        }
        self.enqueue(now, frame);
        true
    }

    /// Holds a control frame (see [`CONTROL_RESERVE_BITS`]): it may use the reserve past the
    /// cap, so a full queue of bulk data doesn't cost the guest its ACK. False (dropped) only
    /// when the reserve is spent too.
    pub fn push_control(&mut self, now: Instant, frame: Vec<u8>) -> bool {
        let fits = queue_bits(self.bucket.rate_bps).is_none_or(|cap| self.held_bits + frame.len() as u64 * 8 <= cap + CONTROL_RESERVE_BITS);
        if !fits {
            return false;
        }
        self.enqueue(now, frame);
        true
    }

    /// The next frame whose time has come.
    pub fn pop_due(&mut self, now: Instant) -> Option<Vec<u8>> {
        if self.queue.front().is_some_and(|h| h.due <= now) {
            let h = self.queue.pop_front()?;
            self.held_bits -= h.frame.len() as u64 * 8;
            return Some(h.frame);
        }
        None
    }

    /// Every held frame, in order, due or not (the shaping ended).
    pub fn drain(&mut self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.held_bits = 0;
        self.last_due = None;
        self.queue.drain(..).map(|h| h.frame)
    }

    /// When the oldest held frame is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.queue.front().map(|h| h.due)
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.held_bits = 0;
        self.last_due = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 1500;

    fn cond(up: f64, down: f64, min: i32, max: i32) -> Conditions {
        Conditions { upload_bps: up, download_bps: down, min_latency_ms: min, max_latency_ms: max, ..Conditions::default() }
    }

    /// An IPv4 frame of `proto` (the rest zero): all `Loss` looks at.
    fn ip(proto: u8) -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[12..14].copy_from_slice(&[0x08, 0x00]);
        f[14] = 0x45;
        f[23] = proto;
        f
    }

    fn arp() -> Vec<u8> {
        let mut f = vec![0u8; 42];
        f[12..14].copy_from_slice(&[0x08, 0x06]);
        f
    }

    /// Which of `n` frames each way a loss drops, as a string of `x` (dropped) and `.`.
    fn pattern(loss: &mut Loss, up: bool, n: usize) -> String {
        let f = ip(super::super::packet::PROTO_UDP);
        (0..n).map(|_| if if up { loss.drop_up(&f) } else { loss.drop_down(&f) } { 'x' } else { '.' }).collect()
    }

    #[test]
    fn a_fixed_seed_drops_a_fixed_pattern_at_0_50_and_100_percent() {
        let mut l = Loss::new(7);
        assert_eq!(pattern(&mut l, true, 64), ".".repeat(64), "no loss by default");
        l.set(0.0, 0.0);
        assert_eq!(pattern(&mut l, false, 64), ".".repeat(64));
        l.set(100.0, 100.0);
        assert_eq!(pattern(&mut l, true, 64), "x".repeat(64));
        assert_eq!(pattern(&mut l, false, 64), "x".repeat(64));
        let half = |seed| {
            let mut l = Loss::new(seed);
            l.set(50.0, 50.0);
            (pattern(&mut l, true, 32), pattern(&mut l, false, 32))
        };
        // The same seed repeats exactly; another seed doesn't.
        assert_eq!(half(7), half(7));
        assert_ne!(half(7), half(8));
        let (up, down) = half(7);
        assert_eq!((up.as_str(), down.as_str()), PATTERN_SEED_7);
        // Near half over many draws.
        let mut l = Loss::new(7);
        l.set(50.0, 10.0);
        let up = pattern(&mut l, true, 10_000).matches('x').count();
        let down = pattern(&mut l, false, 10_000).matches('x').count();
        assert!((4_800..=5_200).contains(&up), "{up}");
        assert!((900..=1_100).contains(&down), "{down}");
    }

    /// Seed 7 at 50% each way: the first 32 decisions up, then down (pinned, so a change to
    /// the generator is a deliberate one).
    const PATTERN_SEED_7: (&str, &str) = ("x....xx......x.x..xxx...xxxxxx..", "xx.xxx..x.x...xx..xx...x...xxxx.");

    #[test]
    fn each_direction_draws_from_its_own_stream() {
        let only_down = {
            let mut l = Loss::new(42);
            l.set(50.0, 50.0);
            pattern(&mut l, false, 200)
        };
        // However much the upload drew first, and at whatever percent, the download's pattern
        // is the same.
        for (up_pct, up_draws) in [(50.0, 0), (50.0, 1), (50.0, 333), (100.0, 50), (0.0, 50)] {
            let mut l = Loss::new(42);
            l.set(up_pct, 50.0);
            pattern(&mut l, true, up_draws);
            assert_eq!(pattern(&mut l, false, 200), only_down, "after {up_draws} upload draws at {up_pct}%");
        }
        // And one direction lossless leaves the other lossy.
        let mut l = Loss::new(42);
        l.set(0.0, 100.0);
        assert_eq!((pattern(&mut l, true, 8), pattern(&mut l, false, 8)), (".".repeat(8), "x".repeat(8)));
    }

    /// TCP, UDP and ICMP packets may be dropped; ARP and anything not IPv4 never are, and
    /// draw nothing (so they don't move the pattern).
    #[test]
    fn only_ip_packets_are_dropped_and_link_control_never() {
        use super::super::packet::{PROTO_ICMP, PROTO_TCP, PROTO_UDP};
        let mut l = Loss::new(1);
        l.set(100.0, 100.0);
        for p in [PROTO_TCP, PROTO_UDP, PROTO_ICMP] {
            assert!(l.drop_up(&ip(p)) && l.drop_down(&ip(p)), "proto {p}");
        }
        let mut ipv6 = ip(PROTO_TCP);
        ipv6[12..14].copy_from_slice(&[0x86, 0xdd]);
        for f in [arp(), ip(2), ipv6, vec![0u8; 10], Vec::new()] {
            assert!(!l.drop_up(&f) && !l.drop_down(&f), "{f:?}");
        }
        let plain = {
            let mut l = Loss::new(3);
            l.set(50.0, 50.0);
            pattern(&mut l, true, 50)
        };
        let mut l = Loss::new(3);
        l.set(50.0, 50.0);
        let mixed: String = (0..50)
            .map(|_| {
                assert!(!l.drop_up(&arp()));
                if l.drop_up(&ip(PROTO_UDP)) { 'x' } else { '.' }
            })
            .collect();
        assert_eq!(mixed, plain, "ARP draws nothing");
    }

    /// Pushes `n` frames at `t0` and returns when the last is due.
    fn last_due(d: &mut Downlink, t0: Instant, n: usize) -> Instant {
        for _ in 0..n {
            d.push(t0, vec![0; FRAME]);
        }
        d.queue.back().expect("pushed").due
    }

    /// Feeds `n` frames as a sender does when the queue is full: it waits for room. Returns
    /// when the last is due.
    fn feed(d: &mut Downlink, t0: Instant, n: usize) -> Instant {
        let (mut now, mut sent) = (t0, 0);
        while sent < n {
            if d.push(now, vec![0; FRAME]) {
                sent += 1;
            } else {
                now = d.next_due().expect("full means held");
                while d.pop_due(now).is_some() {}
            }
        }
        d.queue.back().expect("pushed").due
    }

    #[test]
    fn control_frames_use_a_bounded_reserve_when_bulk_finds_the_queue_full() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 14_400.0, 0, 0));
        while d.push(t0, vec![0; FRAME]) {}
        assert!(!d.push(t0, vec![0; FRAME]), "bulk is refused");
        assert!(d.push_control(t0, vec![0; 60]), "a control frame still fits");
        let cap = queue_bits(14_400.0).expect("rate");
        let mut taken = 0;
        while d.push_control(t0, vec![0; 60]) {
            taken += 1;
            assert!(taken < 100_000, "the reserve is bounded");
        }
        assert!(d.held_bits <= cap + CONTROL_RESERVE_BITS, "{} bits held", d.held_bits);
        assert!(!d.push(t0, vec![0; FRAME]), "bulk stays refused behind the reserve");
        // Unlimited: nothing is ever refused.
        d.set(&cond(0.0, 0.0, 0, 0));
        assert!(d.push_control(t0, vec![0; 60]));
    }

    #[test]
    fn download_rate_holds_per_direction_within_tolerance() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 473_600.0, 0, 0));
        // 200 frames = 2.4 Mbit at 473.6 kbit/s: 5.07 s less the opening burst.
        let secs = feed(&mut d, t0, 200).duration_since(t0).as_secs_f64();
        let ideal = 200.0 * FRAME as f64 * 8.0 / 473_600.0;
        assert!((ideal - 0.05..=ideal).contains(&secs), "{secs} vs {ideal}");
    }

    #[test]
    fn upload_bucket_admits_at_the_rate() {
        let t0 = Instant::now();
        let mut up = Bucket::new();
        up.set_rate(1_000_000.0);
        let (mut now, mut sent) = (t0, 0usize);
        // Send whenever the bucket is not in debt, over 10 simulated seconds.
        while now.duration_since(t0) < Duration::from_secs(10) {
            let w = up.wait(now);
            if w.is_zero() {
                up.take(now, FRAME);
                sent += FRAME;
            } else {
                now += w;
            }
        }
        let bps = sent as f64 * 8.0 / 10.0;
        assert!((900_000.0..=1_030_000.0).contains(&bps), "{bps}");
    }

    #[test]
    fn zero_is_unlimited_and_undelayed() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 0.0, 0, 0));
        for _ in 0..5000 {
            d.push(t0, vec![0; FRAME]);
        }
        let mut n = 0;
        while d.pop_due(t0).is_some() {
            n += 1;
        }
        assert_eq!(n, 5000);
        let mut b = Bucket::new();
        assert_eq!(b.wait(t0), Duration::ZERO);
        assert_eq!(b.take(t0, usize::MAX / 16), Duration::ZERO);
    }

    #[test]
    fn latency_stays_between_min_and_max_and_keeps_order() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 0.0, 35, 200));
        let mut dues = Vec::new();
        for i in 0..500u32 {
            let at = t0 + Duration::from_millis(u64::from(i) * 2);
            d.push(at, vec![i as u8]);
            let due = d.queue.back().expect("pushed").due;
            assert!(due >= at + Duration::from_millis(35), "frame {i} early");
            dues.push(due);
        }
        // Order is kept, and no frame waits past the max after the one before it.
        assert!(dues.windows(2).all(|w| w[0] <= w[1]));
        let first = d.queue.front().expect("held").due;
        assert!(first <= t0 + Duration::from_millis(200));
        // The first frame, undisturbed by one ahead, sits inside [min, max].
        let mut lone = Downlink::new();
        lone.set(&cond(0.0, 0.0, 35, 200));
        lone.push(t0, vec![0]);
        let wait = lone.next_due().expect("held").duration_since(t0);
        assert!((Duration::from_millis(35)..=Duration::from_millis(200)).contains(&wait));
    }

    #[test]
    fn fixed_latency_is_exact() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 0.0, 40, 40));
        d.push(t0, vec![1]);
        assert_eq!(d.next_due(), Some(t0 + Duration::from_millis(40)));
        assert!(d.pop_due(t0 + Duration::from_millis(39)).is_none());
        assert!(d.pop_due(t0 + Duration::from_millis(40)).is_some());
    }

    #[test]
    fn changing_conditions_mid_stream_retimes_held_frames() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 100_000.0, 0, 0));
        let slow = last_due(&mut d, t0, 8).duration_since(t0);
        assert!(slow > Duration::from_millis(800), "{slow:?}");
        // `network speed full`: the backlog is due now.
        let t1 = t0 + Duration::from_millis(100);
        d.set(&cond(0.0, 0.0, 0, 0));
        assert_eq!(d.len(), 8);
        let mut n = 0;
        while d.pop_due(t1).is_some() {
            n += 1;
        }
        assert_eq!(n, 8);
        // And a throttle applied later takes hold for the frames after it.
        d.set(&cond(0.0, 100_000.0, 0, 0));
        let secs = last_due(&mut d, t1, 8).duration_since(t1).as_secs_f64();
        assert!(secs > 0.8, "{secs}");
    }

    #[test]
    fn idle_time_refills_only_a_burst() {
        let t0 = Instant::now();
        let mut b = Bucket::new();
        b.set_rate(100_000.0);
        // Spend the opening burst so the refill clock is running, then go quiet for an hour.
        while b.take(t0, FRAME).is_zero() {}
        // The hour banks no more than the burst (2000 bits here, one frame floor 12144): the
        // third frame after it is already in debt, where an unclamped bucket owes nothing.
        let t1 = t0 + Duration::from_secs(3600);
        let waits: Vec<Duration> = (0..3).map(|_| b.take(t1, FRAME)).collect();
        assert!(waits[0].is_zero(), "the burst covers one frame: {waits:?}");
        assert!(waits[2] > Duration::from_millis(200), "an hour's idle is not 3 frames of credit: {waits:?}");
    }

    #[test]
    fn negative_or_invalid_settings_mean_unshaped() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(-5.0, f64::NAN, -10, -1));
        d.push(t0, vec![0; FRAME]);
        assert_eq!(d.next_due(), Some(t0));
    }

    fn secs(t: Instant, from: Instant) -> f64 {
        t.duration_since(from).as_secs_f64()
    }

    #[test]
    fn a_new_rate_times_held_frames_once_not_on_top_of_their_first_charge() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 100_000.0, 0, 0));
        // 8 frames = 96 kbit: 0.96 s at 100 kbit/s; the rate doubles to 200 kbit/s.
        for _ in 0..8 {
            assert!(d.push(t0, vec![0; FRAME]), "fits in the second's queue? {}", d.len());
        }
        d.set(&cond(0.0, 200_000.0, 0, 0));
        // As if 200 kbit/s had always applied: 0.48 s (less the opening burst), not the old
        // 0.96 s of debt kept and the 8 frames charged again on top.
        let last = secs(d.queue.back().expect("held").due, t0);
        assert!((0.38..=0.49).contains(&last), "{last}");
        assert_eq!(d.len(), 8);
    }

    #[test]
    fn a_new_latency_alone_leaves_held_frames_their_place_in_the_rate() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 100_000.0, 0, 0));
        for _ in 0..8 {
            d.push(t0, vec![0; FRAME]);
        }
        let before = secs(d.queue.back().expect("held").due, t0);
        // `network delay` alone: 100 ms on top, nothing more.
        d.set(&cond(0.0, 100_000.0, 100, 100));
        let after = secs(d.queue.back().expect("held").due, t0);
        assert!((after - before - 0.1).abs() < 0.001, "{before} -> {after}");
        // And back to none.
        d.set(&cond(0.0, 100_000.0, 0, 0));
        assert!((secs(d.queue.back().expect("held").due, t0) - before).abs() < 0.001);
    }

    #[test]
    fn a_burst_at_gsm_never_queues_more_than_a_second() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        let gsm = 14_400.0;
        d.set(&cond(0.0, gsm, 0, 0));
        // A sender that dumps 500 frames at once: what is held stays within a second of the rate
        // (two frames' worth at this speed), and the rest is refused.
        let kept = (0..500).filter(|_| d.push(t0, vec![0; FRAME])).count();
        assert!((1..=3).contains(&kept), "{kept}");
        let last = secs(d.queue.back().expect("held").due, t0);
        assert!(last <= 1.0 + FRAME as f64 * 8.0 / gsm, "{last} s of queue");
        // Room tells the engine how much the next pass may add, and shrinks as frames queue.
        assert_eq!(d.room(), 0);
        assert!(d.capacity() >= 2);
        // Draining makes room again.
        while d.pop_due(t0 + Duration::from_secs(5)).is_some() {}
        assert!(d.room() >= 2);
    }

    #[test]
    fn the_queue_holds_a_seconds_of_a_fast_rate_and_none_is_unbounded() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(&cond(0.0, 1_000_000.0, 0, 0));
        let kept = (0..1000).filter(|_| d.push(t0, vec![0; FRAME])).count();
        // 1 Mbit is 83 full-size frames of 1514 bytes; ours are 1500 bytes: 83.
        assert!((80..=85).contains(&kept), "{kept}");
        d.set(&cond(0.0, 0.0, 0, 0));
        assert_eq!(d.room(), usize::MAX);
        assert!((0..5000).all(|_| d.push(t0, vec![0; FRAME])));
    }
}
