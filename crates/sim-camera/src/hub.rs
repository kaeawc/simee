//! Fan-out: one capture per source, shared by every subscriber linked to it. The first
//! subscription opens the source (outside the hub's lock, so a slow open blocks only its own
//! source) and starts its capture thread; the capture runs at the highest rate its subscribers ask
//! for and hands each subscriber frames at its own size and rate: a synthetic source draws one
//! frame per distinct size (a still or a same-size frame is shared, never copied), a camera's one
//! frame is fitted to each size. The capture stops, dropping the source and releasing any camera,
//! as soon as the last subscription is dropped.
//!
//! [`Hub::events`] is the hub's event stream: cameras added and removed (debounced host
//! notifications, then a fresh listing diffed against the last), and captures whose source went
//! missing mid-stream. The host watch starts with the first stream and stops with the last.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::catalog::{self, HostCamera};
use crate::error::{Error, Result};
use crate::frame::{CameraFrame, Fitted, FrameSpec, Tick};
use crate::hotplug::{Debounce, Event, HostHotPlug, HotPlug, MAX_DELAY, QUIET, Signals};
use crate::id::SourceSpec;
use crate::source::{Opener, Source};

/// Frames a slow subscriber may fall behind by before newer frames are dropped for it (the
/// capture never waits for a subscriber).
const QUEUE: usize = 2;

/// How often the hot-plug watch checks, with no signals, whether anyone still listens.
const IDLE: Duration = Duration::from_millis(250);

/// The captures running now, keyed by canonical source id.
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    opener: Box<dyn Opener>,
    captures: Mutex<HashMap<String, Entry>>,
    hotplug: Box<dyn HotPlug>,
    watch: Mutex<Watch>,
}

/// The event streams open now, and whether the host watch runs for them.
#[derive(Default)]
struct Watch {
    listeners: Vec<(u64, Sender<Event>)>,
    next_id: u64,
    running: bool,
    /// Why this generation of the watch stopped, when it failed: what each of its streams' next
    /// receive returns. A restart gets a fresh cell, so it can't overwrite a cause a stream of the
    /// failed watch hasn't read yet.
    cause: Cause,
}

type Cause = Arc<Mutex<Option<Error>>>;

/// A source's slot in the hub: being opened (by one subscriber, outside the lock) or running.
#[derive(Clone)]
enum Entry {
    Opening(Arc<Gate>),
    Running(Arc<Capture>),
}

/// Released when an open finishes, either way; subscribers to the same source wait on it.
#[derive(Default)]
struct Gate {
    done: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut done = lock(&self.done);
        while !*done {
            done = self.cv.wait(done).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn finish(&self) {
        *lock(&self.done) = true;
        self.cv.notify_all();
    }
}

/// The right to open one source. Dropping it (also on a panic in the open) clears the slot if it
/// is still the opening one, and releases the subscribers waiting for it.
struct Claim<'a> {
    inner: &'a Inner,
    key: &'a str,
    gate: Arc<Gate>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut captures = lock(&self.inner.captures);
        if matches!(captures.get(self.key), Some(Entry::Opening(g)) if Arc::ptr_eq(g, &self.gate)) {
            captures.remove(self.key);
        }
        drop(captures);
        self.gate.finish();
    }
}

struct Capture {
    key: String,
    state: Mutex<State>,
    wake: Condvar,
}

struct State {
    subscribers: Vec<Subscriber>,
    next_id: u64,
    /// Set once the capture thread has decided to exit; a new subscription then starts a new one.
    stopped: bool,
    /// Why the capture ended, when it failed: what every subscriber's next receive returns.
    failure: Option<Error>,
}

struct Subscriber {
    id: u64,
    spec: FrameSpec,
    tx: SyncSender<CameraFrame>,
    due: Instant,
}

/// A running capture, for listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Active {
    pub source: String,
    pub subscribers: usize,
}

impl Hub {
    pub fn new(opener: impl Opener + 'static) -> Hub {
        Hub::with_hotplug(opener, HostHotPlug)
    }

    /// A hub whose hot-plug events come from `hotplug` (fakes in tests).
    pub fn with_hotplug(opener: impl Opener + 'static, hotplug: impl HotPlug + 'static) -> Hub {
        Hub {
            inner: Arc::new(Inner {
                opener: Box::new(opener),
                captures: Mutex::new(HashMap::new()),
                hotplug: Box::new(hotplug),
                watch: Mutex::new(Watch::default()),
            }),
        }
    }

    /// The hub's events from now on: cameras added and removed, and captures whose source went
    /// missing. The first stream starts watching the host (and fails here when it can't); the
    /// watch stops when the last stream is dropped.
    pub fn events(&self) -> Result<Events> {
        let mut watch = lock(&self.inner.watch);
        if !watch.running {
            // Watch first, then list: a change between the two is still signalled.
            let signals = self.inner.hotplug.watch()?;
            let known = self.inner.hotplug.cameras()?;
            let weak = Arc::downgrade(&self.inner);
            thread::Builder::new()
                .name("camera hot-plug".into())
                .spawn(move || run_watch(weak, signals, known))
                .map_err(|e| Error::Capture(format!("can't start the hot-plug thread: {e}")))?;
            watch.running = true;
            watch.cause = Cause::default();
        }
        let (tx, rx) = mpsc::channel();
        let id = watch.next_id;
        watch.next_id += 1;
        watch.listeners.push((id, tx));
        Ok(Events { id, inner: self.inner.clone(), rx, cause: watch.cause.clone() })
    }

    /// Subscribes to `source` at `spec`. Opens the source (and fails here, with the reason, when
    /// it can't: a missing camera, the permission) unless a capture of it is already running.
    pub fn subscribe(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Subscription> {
        let canonical = self.inner.opener.canonical(source)?;
        let key = canonical.to_string();
        let gate = loop {
            let mut captures = lock(&self.inner.captures);
            match captures.get(&key).cloned() {
                Some(Entry::Running(capture)) => {
                    let mut state = lock(&capture.state);
                    if !state.stopped {
                        let (id, rx) = add(&mut state, spec);
                        drop(state);
                        capture.wake.notify_all();
                        return Ok(Subscription { id, spec, capture, rx });
                    }
                }
                // Another subscriber is opening this source: wait, then join its capture (or open
                // it ourselves if that failed).
                Some(Entry::Opening(gate)) => {
                    drop(captures);
                    gate.wait();
                    continue;
                }
                None => {}
            }
            let gate = Arc::new(Gate::default());
            captures.insert(key.clone(), Entry::Opening(gate.clone()));
            break gate;
        };
        let claim = Claim { inner: &self.inner, key: &key, gate };
        // Opened without the hub's lock: a slow open (a FIFO, a stalled mount, a camera starting)
        // holds up only subscribers to this source.
        let source = self.inner.opener.open(&canonical, spec)?;
        let capture = Arc::new(Capture {
            key: key.clone(),
            state: Mutex::new(State { subscribers: Vec::new(), next_id: 0, stopped: false, failure: None }),
            wake: Condvar::new(),
        });
        let (id, rx) = add(&mut lock(&capture.state), spec);
        lock(&self.inner.captures).insert(key.clone(), Entry::Running(capture.clone()));
        drop(claim);
        let (weak, run) = (Arc::downgrade(&self.inner), capture.clone());
        // Named by kind only: the id can be long, or hold a NUL (a `qr:` text), which a thread
        // name can't.
        let name = format!("camera {}", canonical.kind());
        let spawned = thread::Builder::new().name(name).spawn(move || run_capture(weak, run, source));
        if let Err(e) = spawned {
            remove(&Arc::downgrade(&self.inner), &capture);
            return Err(Error::Capture(format!("can't start the capture thread: {e}")));
        }
        Ok(Subscription { id, spec, capture, rx })
    }

    /// The captures running now, by source id.
    pub fn active(&self) -> Vec<Active> {
        let captures = lock(&self.inner.captures);
        let mut out: Vec<Active> = captures
            .values()
            .filter_map(|e| match e {
                Entry::Running(c) => Some(Active { source: c.key.clone(), subscribers: lock(&c.state).subscribers.len() }),
                Entry::Opening(_) => None,
            })
            .filter(|a| a.subscribers > 0)
            .collect();
        out.sort_by(|a, b| a.source.cmp(&b.source));
        out
    }
}

/// Sends `event` to every event stream, dropping streams that are gone.
fn broadcast(inner: &Inner, event: &Event) {
    lock(&inner.watch).listeners.retain(|(_, tx)| tx.send(event.clone()).is_ok());
}

/// The hot-plug thread: debounced host signals, each burst a fresh listing diffed against the
/// last. Ends when the last stream goes (checked every `IDLE`) or the host watch fails.
fn run_watch(hub: Weak<Inner>, mut signals: Box<dyn Signals>, mut known: Vec<HostCamera>) {
    let mut debounce = Debounce::new(QUIET, MAX_DELAY);
    loop {
        let signalled = signals.wait(debounce.timeout(Instant::now(), IDLE));
        let Some(inner) = hub.upgrade() else { return };
        match signalled {
            Ok(true) => debounce.signal(Instant::now()),
            Ok(false) => {}
            Err(e) => {
                let mut watch = lock(&inner.watch);
                // Dropping the senders ends every stream; each then reads `failure`.
                watch.running = false;
                *lock(&watch.cause) = Some(e);
                watch.listeners.clear();
                return;
            }
        }
        {
            let mut watch = lock(&inner.watch);
            if watch.listeners.is_empty() {
                watch.running = false;
                return;
            }
        }
        // A listing that fails (a node udev hasn't opened up yet) waits for the next signal.
        if debounce.take(Instant::now())
            && let Ok(now) = inner.hotplug.cameras()
        {
            for change in catalog::changes(&known, &now) {
                broadcast(&inner, &Event::from(change));
            }
            known = now;
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn add(state: &mut State, spec: FrameSpec) -> (u64, Receiver<CameraFrame>) {
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let id = state.next_id;
    state.next_id += 1;
    state.subscribers.push(Subscriber { id, spec, tx, due: Instant::now() });
    (id, rx)
}

/// One consumer's link to a capture. Dropping it unsubscribes; the capture stops with the last.
pub struct Subscription {
    id: u64,
    spec: FrameSpec,
    capture: Arc<Capture>,
    rx: Receiver<CameraFrame>,
}

impl Subscription {
    /// The canonical id of the source (`webcam:<id>` for a webcam however it was named).
    pub fn source(&self) -> &str {
        &self.capture.key
    }

    pub fn spec(&self) -> FrameSpec {
        self.spec
    }

    /// The next frame, waiting up to `timeout`; `Ok(None)` on a timeout. Once the capture has
    /// failed (the camera was unplugged): the frames already queued, then the failure.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<CameraFrame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    /// A frame if one is waiting.
    pub fn try_recv(&self) -> Result<Option<CameraFrame>> {
        match self.rx.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(self.ended()),
        }
    }

    fn ended(&self) -> Error {
        let failure = lock(&self.capture.state).failure.clone();
        failure.unwrap_or_else(|| Error::Capture(format!("the capture of {} stopped", self.capture.key)))
    }
}

/// One consumer's stream of hub events. Dropping it unlistens; the host watch stops with the last.
pub struct Events {
    id: u64,
    inner: Arc<Inner>,
    rx: Receiver<Event>,
    cause: Cause,
}

impl Events {
    /// The next event, waiting up to `timeout`; `Ok(None)` on a timeout. Once the host watch has
    /// failed: the events already queued, then the failure.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Event>> {
        match self.rx.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    /// An event if one is waiting.
    pub fn try_recv(&self) -> Result<Option<Event>> {
        match self.rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(self.ended()),
        }
    }

    fn ended(&self) -> Error {
        let failure = lock(&self.cause).clone();
        failure.unwrap_or_else(|| Error::Capture("the hot-plug watch stopped".into()))
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        lock(&self.inner.watch).listeners.retain(|(id, _)| *id != self.id);
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.capture.state).subscribers.retain(|s| s.id != self.id);
        self.capture.wake.notify_all();
    }
}

/// The capture spec for a set of subscribers: the largest frame asked for (what a camera is
/// fitted from), at the highest rate.
fn capture_spec(subscribers: &[Subscriber]) -> Option<FrameSpec> {
    let largest = subscribers.iter().map(|s| s.spec).max_by_key(FrameSpec::area)?;
    let fps = subscribers.iter().map(|s| s.spec.fps).max().unwrap_or(largest.fps);
    Some(FrameSpec { fps, ..largest })
}

fn run_capture(hub: Weak<Inner>, capture: Arc<Capture>, mut source: Box<dyn Source>) {
    let started = Instant::now();
    let wall = SystemTime::now();
    let synthetic = source.synthetic();
    let mut seq = 0u64;
    let mut next = started;
    let mut fits = Fits::default();
    loop {
        // Sleep until the next tick, waking early when a subscriber comes or goes.
        let due_now = {
            let mut state = lock(&capture.state);
            loop {
                if state.subscribers.is_empty() {
                    break None;
                }
                let now = Instant::now();
                if now >= next {
                    let wanted = state.subscribers.iter().map(|s| (s.spec.width, s.spec.height)).collect::<Vec<_>>();
                    break capture_spec(&state.subscribers).map(|spec| (spec, take_due(&mut state.subscribers, now), wanted));
                }
                state = capture.wake.wait_timeout(state, next - now).map(|(s, _)| s).unwrap_or_else(|p| p.into_inner().0);
            }
        };
        let Some((spec, due, wanted)) = due_now else {
            if stop_if_idle(&hub, &capture) {
                return;
            }
            continue;
        };
        let now = Instant::now();
        let pts = now - started;
        let tick = Tick { seq, pts, wall: wall + pts };
        let frames = render(source.as_mut(), synthetic, spec, tick, &due, &wanted, &mut fits);
        seq += 1;
        next = (next + spec.interval()).max(now);
        let mut state = lock(&capture.state);
        match frames {
            Ok(frames) => deliver(&mut state.subscribers, &frames, &due),
            Err(e) => {
                // Dropping the senders ends every subscriber's stream; each then reads `failure`.
                state.failure = Some(e.clone());
                state.stopped = true;
                state.subscribers.clear();
                drop(state);
                remove(&hub, &capture);
                if matches!(e, Error::Missing(_))
                    && let Some(inner) = hub.upgrade()
                {
                    broadcast(&inner, &Event::SourceMissing { source: capture.key.clone(), error: e });
                }
                return;
            }
        }
    }
}

/// The subscribers due a frame now, as (id, size); moves their next due time on.
fn take_due(subscribers: &mut [Subscriber], now: Instant) -> Vec<(u64, (u32, u32))> {
    let mut due = Vec::new();
    for s in subscribers.iter_mut().filter(|s| now >= s.due) {
        s.due = (s.due + s.spec.interval()).max(now);
        due.push((s.id, (s.spec.width, s.spec.height)));
    }
    due
}

/// A capture's last source frame fitted to its subscribers' other sizes: a source that repeats a
/// frame (a video file slower than the capture) shares its fits instead of scaling again.
#[derive(Default)]
struct Fits {
    /// The source pixels the fits are of; held, so a new frame can't reuse their address.
    source: Option<Arc<[u8]>>,
    /// The source frame's size: the same pixels at another size are another picture.
    size: (u32, u32),
    sizes: Fitted,
}

impl Fits {
    fn fit(&mut self, frame: &CameraFrame, width: u32, height: u32) -> CameraFrame {
        if (width, height) == (frame.width, frame.height) {
            return frame.clone();
        }
        let size = (frame.width, frame.height);
        if !self.source.as_ref().is_some_and(|s| Arc::ptr_eq(s, &frame.bgra)) || self.size != size {
            (self.source, self.size) = (Some(frame.bgra.clone()), size);
            self.sizes.clear();
        }
        let bgra = match self.sizes.iter().find(|(size, _)| *size == (width, height)) {
            Some((_, bgra)) => bgra.clone(),
            None => {
                let bgra = frame.fit(width, height).bgra;
                self.sizes.push(((width, height), bgra.clone()));
                bgra
            }
        };
        CameraFrame { width, height, bgra, tick: frame.tick }
    }
}

/// One frame per distinct size among the due subscribers. A synthetic source draws each size
/// itself; a camera is asked for its frame once (every tick, so a stalled camera is noticed) and
/// that frame is fitted to the other sizes. Fits are kept for the sizes in `wanted` (every
/// subscriber's, due or not) and dropped for the rest.
fn render(
    source: &mut dyn Source,
    synthetic: bool,
    spec: FrameSpec,
    tick: Tick,
    due: &[(u64, (u32, u32))],
    wanted: &[(u32, u32)],
    fits: &mut Fits,
) -> Result<Vec<CameraFrame>> {
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    for &(_, size) in due {
        if !sizes.contains(&size) {
            sizes.push(size);
        }
    }
    if synthetic {
        return sizes.into_iter().map(|(width, height)| source.frame(FrameSpec { width, height, ..spec }, tick)).collect();
    }
    let frame = source.frame(spec, tick)?;
    // Only sizes a subscriber still asks for, due this tick or not, keep their fits.
    fits.sizes.retain(|(size, _)| wanted.contains(size));
    Ok(sizes.into_iter().map(|(width, height)| fits.fit(&frame, width, height)).collect())
}

/// Sends each due subscriber the frame of its size; drops subscribers whose subscription is gone.
fn deliver(subscribers: &mut Vec<Subscriber>, frames: &[CameraFrame], due: &[(u64, (u32, u32))]) {
    subscribers.retain(|s| {
        let Some(&(_, size)) = due.iter().find(|(id, _)| *id == s.id) else { return true };
        let Some(frame) = frames.iter().find(|f| (f.width, f.height) == size) else { return true };
        !matches!(s.tx.try_send(frame.clone()), Err(TrySendError::Disconnected(_)))
    });
}

/// Ends the capture if it still has no subscribers (checked under the hub's lock, so a racing
/// subscribe either joins before this or starts a fresh capture after).
fn stop_if_idle(hub: &Weak<Inner>, capture: &Arc<Capture>) -> bool {
    let Some(inner) = hub.upgrade() else { return true };
    let mut captures = lock(&inner.captures);
    let mut state = lock(&capture.state);
    if !state.subscribers.is_empty() {
        return false;
    }
    state.stopped = true;
    if matches!(captures.get(&capture.key), Some(Entry::Running(c)) if Arc::ptr_eq(c, capture)) {
        captures.remove(&capture.key);
    }
    true
}

fn remove(hub: &Weak<Inner>, capture: &Arc<Capture>) {
    if let Some(inner) = hub.upgrade() {
        let mut captures = lock(&inner.captures);
        if matches!(captures.get(&capture.key), Some(Entry::Running(c)) if Arc::ptr_eq(c, capture)) {
            captures.remove(&capture.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::synth;

    /// Counts opens and drops; frames are the synthetic source's, or an error after `fail_after`
    /// frames (a webcam's is an unplug).
    #[derive(Clone, Default)]
    struct Counts {
        opened: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
        frames: Arc<AtomicUsize>,
    }

    struct Fake {
        counts: Counts,
        fail_after: Option<usize>,
    }

    struct Counted {
        inner: Box<dyn Source>,
        counts: Counts,
        fail_after: Option<usize>,
        webcam: bool,
    }

    impl Source for Counted {
        fn synthetic(&self) -> bool {
            self.inner.synthetic()
        }

        fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let n = self.counts.frames.fetch_add(1, Ordering::SeqCst);
            if self.fail_after.is_some_and(|max| n >= max) {
                if self.webcam {
                    return Err(Error::Missing("the camera was unplugged".into()));
                }
                return Err(Error::Capture("unplugged".into()));
            }
            self.inner.frame(spec, tick)
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.counts.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Opener for Fake {
        fn canonical(&self, spec: &SourceSpec) -> Result<SourceSpec> {
            // "webcam:any" plays a webcam with two spellings.
            match spec {
                SourceSpec::Webcam(_) => Ok(SourceSpec::Webcam("the-id".into())),
                other => Ok(other.clone()),
            }
        }

        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> Result<Box<dyn Source>> {
            let inner = match source {
                SourceSpec::Webcam(_) => synth::open(&SourceSpec::Pattern(crate::id::Pattern::Testcard))?,
                other => synth::open(other)?,
            };
            self.counts.opened.fetch_add(1, Ordering::SeqCst);
            let webcam = matches!(source, SourceSpec::Webcam(_));
            Ok(Box::new(Counted { inner, counts: self.counts.clone(), fail_after: self.fail_after, webcam }))
        }
    }

    fn hub(fail_after: Option<usize>) -> (Hub, Counts) {
        let counts = Counts::default();
        (Hub::new(Fake { counts: counts.clone(), fail_after }), counts)
    }

    fn spec(w: u32, h: u32, fps: u32) -> FrameSpec {
        FrameSpec::new(w, h, fps).unwrap_or_default()
    }

    fn parse(id: &str) -> SourceSpec {
        SourceSpec::parse(id).unwrap_or_else(|e| panic!("{id}: {e}"))
    }

    fn next(sub: &Subscription) -> CameraFrame {
        match sub.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(f)) => f,
            other => panic!("no frame from {}: {other:?}", sub.source()),
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn subscribers_of_one_source_share_one_capture_and_its_pixels() {
        let (hub, counts) = hub(None);
        let a = hub.subscribe(&parse("solid:red"), spec(64, 48, 60)).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("solid:red"), spec(64, 48, 60)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
        assert_eq!(hub.active(), vec![Active { source: "solid:red".into(), subscribers: 2 }]);
        let (fa, fb) = (next(&a), next(&b));
        assert!(Arc::ptr_eq(&fa.bgra, &fb.bgra), "one frame, shared");
        assert_eq!(fa.pixel(0, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn two_spellings_of_one_webcam_share_a_capture() {
        let (hub, counts) = hub(None);
        let a = hub.subscribe(&parse("webcam:obs"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("OBS Virtual Camera"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
        assert_eq!((a.source(), b.source()), ("webcam:the-id", "webcam:the-id"));
    }

    #[test]
    fn the_capture_stops_with_the_last_subscriber_and_restarts_on_demand() {
        let (hub, counts) = hub(None);
        let a = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 120)).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 120)).unwrap_or_else(|e| panic!("{e}"));
        next(&a);
        drop(a);
        next(&b);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 0, "still one subscriber");
        drop(b);
        wait_until("the source to be dropped", || counts.dropped.load(Ordering::SeqCst) == 1);
        assert!(hub.active().is_empty());

        let c = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 120)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 2, "a fresh capture");
        let first = next(&c);
        assert_eq!(crate::synth::testcard_seq(&first), Some(first.tick.seq as u32));
    }

    #[test]
    fn different_sources_get_their_own_captures() {
        let (hub, counts) = hub(None);
        let _a = hub.subscribe(&parse("solid:red"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        let _b = hub.subscribe(&parse("qr:hello"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        // An id a thread name couldn't hold.
        let _c = hub.subscribe(&SourceSpec::Qr("a\0b".into()), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 3);
        assert_eq!(hub.active().len(), 3);
    }

    #[test]
    fn each_subscriber_gets_its_own_size() {
        let (hub, _) = hub(None);
        let big = hub.subscribe(&parse("pattern:testcard"), spec(640, 480, 60)).unwrap_or_else(|e| panic!("{e}"));
        let small = hub.subscribe(&parse("pattern:testcard"), spec(320, 240, 60)).unwrap_or_else(|e| panic!("{e}"));
        // Frames already queued for `big` before `small` joined were at 640x480 either way.
        let (fb, fs) = (next(&big), next(&small));
        assert_eq!((fb.width, fb.height), (640, 480));
        assert_eq!((fs.width, fs.height), (320, 240));
        assert_eq!(crate::synth::testcard_seq(&fs), Some(fs.tick.seq as u32), "scaled from the capture frame");
    }

    /// A camera-like source whose frame doesn't change (a paused or repeated video frame), at its
    /// size.
    struct Same(Arc<[u8]>, (u32, u32));

    impl Source for Same {
        fn frame(&mut self, _spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let (width, height) = self.1;
            Ok(CameraFrame { width, height, bgra: self.0.clone(), tick })
        }
    }

    #[test]
    fn a_repeated_source_frame_is_fitted_once_per_subscriber_size() {
        let mut same = Same(vec![9u8; 4 * 4 * 4].into(), (4, 4));
        let due = [(1, (4, 4)), (2, (2, 2))];
        let wanted = [(4, 4), (2, 2)];
        let mut fits = Fits::default();
        let mut tick = Tick::start();
        let a = render(&mut same, false, spec(4, 4, 30), tick, &due, &wanted, &mut fits).unwrap_or_else(|e| panic!("{e}"));
        tick.seq += 1;
        let b = render(&mut same, false, spec(4, 4, 30), tick, &due, &wanted, &mut fits).unwrap_or_else(|e| panic!("{e}"));
        assert!(Arc::ptr_eq(&a[0].bgra, &same.0) && Arc::ptr_eq(&b[0].bgra, &same.0), "the capture size is the source's own pixels");
        assert_eq!((b[1].width, b[1].height, b[1].tick.seq), (2, 2, 1));
        assert!(Arc::ptr_eq(&a[1].bgra, &b[1].bgra), "the smaller subscriber's fit is reused, not redone");
        same.0 = vec![7u8; 4 * 4 * 4].into();
        let c = render(&mut same, false, spec(4, 4, 30), tick, &due, &wanted, &mut fits).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c[1].pixel(0, 0)[0], 7, "a new source frame is fitted afresh");
    }

    #[test]
    fn fits_for_sizes_no_subscriber_wants_are_dropped() {
        let mut same = Same(vec![9u8; 8 * 8 * 4].into(), (8, 8));
        let mut fits = Fits::default();
        let tick = Tick::start();
        let all = [(1, (8, 8)), (2, (4, 4)), (3, (2, 2)), (4, (1, 1))];
        let a = render(&mut same, false, spec(8, 8, 30), tick, &all, &[(8, 8), (4, 4), (2, 2), (1, 1)], &mut fits)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(fits.sizes.len(), 3);
        // The 2x2 and 1x1 subscribers have gone; the 4x4 one is still there but not due this tick.
        let still = [(8, 8), (4, 4)];
        let b = render(&mut same, false, spec(8, 8, 30), tick, &all[..1], &still, &mut fits).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b.len(), 1);
        let kept: Vec<(u32, u32)> = fits.sizes.iter().map(|(size, _)| *size).collect();
        assert_eq!(kept, [(4, 4)], "only the fit a subscriber still wants is kept");
        let c = render(&mut same, false, spec(8, 8, 30), tick, &all[1..2], &still, &mut fits).unwrap_or_else(|e| panic!("{e}"));
        assert!(Arc::ptr_eq(&a[1].bgra, &c[0].bgra), "and it is reused, not redone");
    }

    #[test]
    fn the_same_pixels_at_another_size_are_fitted_afresh() {
        // Pixel `i` of value `16 * i`: a 4x4 frame and an 8x2 frame of the same bytes differ.
        let bgra: Arc<[u8]> = (0..16u8).flat_map(|i| [16 * i, 16 * i, 16 * i, 255]).collect::<Vec<u8>>().into();
        let mut same = Same(bgra.clone(), (4, 4));
        let mut fits = Fits::default();
        let tick = Tick::start();
        let square = render(&mut same, false, spec(4, 4, 30), tick, &[(1, (2, 2))], &[(2, 2)], &mut fits).unwrap_or_else(|e| panic!("{e}"));
        same.1 = (8, 2);
        let wide = render(&mut same, false, spec(8, 2, 30), tick, &[(1, (2, 2))], &[(2, 2)], &mut fits).unwrap_or_else(|e| panic!("{e}"));
        let want = CameraFrame { width: 8, height: 2, bgra, tick }.fit(2, 2).bgra;
        assert_ne!(&*square[0].bgra, &*want, "the two sizes fit differently");
        assert_eq!(&*wide[0].bgra, &*want, "the 8x2 frame is fitted as itself, not as the 4x4 one");
    }

    /// What a QR frame at `w`x`h` must be: the source drawn at exactly that size.
    fn drawn(id: &str, w: u32, h: u32, tick: Tick) -> CameraFrame {
        let mut source = synth::open(&parse(id)).unwrap_or_else(|e| panic!("{e}"));
        source.frame(spec(w, h, 30), tick).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_qr_code_keeps_its_finder_patterns_and_quiet_zone_at_mixed_aspect_ratios() {
        let (hub, _) = hub(None);
        let wide = hub.subscribe(&parse("qr:hello"), spec(1280, 720, 60)).unwrap_or_else(|e| panic!("{e}"));
        let tall = hub.subscribe(&parse("qr:hello"), spec(720, 1280, 60)).unwrap_or_else(|e| panic!("{e}"));
        let qr = qrcodegen::QrCode::encode_text("hello", qrcodegen::QrCodeEcc::Medium).unwrap_or_else(|e| panic!("{e}"));
        for (sub, (w, h)) in [(&wide, (1280, 720)), (&tall, (720, 1280))] {
            let f = next(sub);
            assert_eq!((f.width, f.height), (w, h));
            let (module, x0, y0) = synth::qr_layout(qr.size(), w, h).unwrap_or_default();
            let (black, white) = ([0, 0, 0, 255], [255, 255, 255, 255]);
            let last = qr.size() - 1;
            for (mx, my) in [(0, 0), (last, 0), (0, last)] {
                let (px, py) = (x0 + mx as u32 * module + module / 2, y0 + my as u32 * module + module / 2);
                assert_eq!(f.pixel(px, py), black, "finder corner {mx},{my} at {w}x{h}");
            }
            assert_eq!(f.pixel(x0 - module / 2, y0 + module / 2), white, "quiet zone at {w}x{h}");
            assert_eq!(f.bgra, drawn("qr:hello", w, h, f.tick).bgra, "the code as drawn at {w}x{h}");
        }
    }

    #[test]
    fn the_test_card_counter_reads_at_mixed_aspect_ratios() {
        let (hub, _) = hub(None);
        let wide = hub.subscribe(&parse("pattern:testcard"), spec(1280, 720, 60)).unwrap_or_else(|e| panic!("{e}"));
        let std = hub.subscribe(&parse("pattern:testcard"), spec(640, 480, 60)).unwrap_or_else(|e| panic!("{e}"));
        for sub in [&wide, &std] {
            // Past the first frames, where seq's high bits matter little but its low ones do.
            for _ in 0..3 {
                let f = next(sub);
                assert_eq!(synth::testcard_seq(&f), Some(f.tick.seq as u32), "{}x{}", f.width, f.height);
            }
        }
    }

    /// Opens block on `release` for `image:` sources; everything else opens at once.
    struct Slow {
        release: Mutex<Receiver<()>>,
        opened: Arc<AtomicUsize>,
    }

    impl Opener for Slow {
        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> Result<Box<dyn Source>> {
            if matches!(source, SourceSpec::Image(_)) {
                let _ = lock(&self.release).recv();
                self.opened.fetch_add(1, Ordering::SeqCst);
                return synth::open(&SourceSpec::Solid("blue".into()));
            }
            synth::open(source)
        }
    }

    #[test]
    fn a_slow_open_blocks_only_its_own_source() {
        let (release, rx) = mpsc::channel();
        let opened = Arc::new(AtomicUsize::new(0));
        let hub = Arc::new(Hub::new(Slow { release: Mutex::new(rx), opened: opened.clone() }));
        let slow = SourceSpec::Image("/stalled/fifo".into());
        let joiners: Vec<_> = (0..2)
            .map(|_| {
                let (hub, slow) = (hub.clone(), slow.clone());
                thread::spawn(move || hub.subscribe(&slow, FrameSpec::default()))
            })
            .collect();
        thread::sleep(Duration::from_millis(100));
        let (done_tx, done_rx) = mpsc::channel();
        let other = hub.clone();
        thread::spawn(move || {
            let r = other.subscribe(&parse("solid:red"), FrameSpec::default()).map(|s| next(&s).pixel(0, 0));
            let _ = done_tx.send((r, other.active()));
        });
        let other_result = done_rx.recv_timeout(Duration::from_secs(3));
        // Let the stalled open finish whatever happened above, so no thread is left blocked.
        let _ = release.send(());
        let (pixel, active) = other_result.unwrap_or_else(|_| panic!("a subscribe to another source waited on the stalled open"));
        assert_eq!(pixel.unwrap_or_default(), [0, 0, 255, 255]);
        assert!(active.iter().all(|a| a.source != "image:/stalled/fifo"), "{active:?}");
        let subs: Vec<Subscription> =
            joiners.into_iter().map(|j| j.join().unwrap_or_else(|_| panic!("panicked")).unwrap_or_else(|e| panic!("{e}"))).collect();
        assert!(subs.iter().all(|s| s.source() == "image:/stalled/fifo"));
        assert_eq!(opened.load(Ordering::SeqCst), 1, "the two subscribers to the slow source shared one open");
    }

    #[test]
    fn a_failed_open_frees_the_slot_for_the_next_subscriber() {
        let (hub, counts) = hub(None);
        assert!(hub.subscribe(&parse("video:/x.mp4"), FrameSpec::default()).is_err());
        assert!(hub.subscribe(&parse("video:/x.mp4"), FrameSpec::default()).is_err());
        let ok = hub.subscribe(&parse("solid:red"), FrameSpec::default());
        assert!(ok.is_ok());
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_slower_subscriber_gets_fewer_frames() {
        let (hub, _) = hub(None);
        let fast = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        let slow = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 10)).unwrap_or_else(|e| panic!("{e}"));
        let (mut n_fast, mut n_slow) = (0, 0);
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            n_fast += usize::from(matches!(fast.try_recv(), Ok(Some(_))));
            n_slow += usize::from(matches!(slow.try_recv(), Ok(Some(_))));
            thread::sleep(Duration::from_millis(2));
        }
        // Loose bounds: a loaded CI host runs the capture thread late, but never lets the 10 fps
        // subscriber see more than its share.
        assert!((1..=10).contains(&n_slow), "slow got {n_slow}");
        assert!(n_fast > n_slow, "fast got {n_fast}, slow {n_slow}");
    }

    #[test]
    fn a_failing_capture_tells_every_subscriber_and_stops() {
        let (hub, counts) = hub(Some(2));
        let a = hub.subscribe(&parse("solid:blue"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("solid:blue"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        let failure = |s: &Subscription| loop {
            match s.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(_)) => continue,
                other => break other,
            }
        };
        assert_eq!(failure(&a), Err(Error::Capture("unplugged".into())));
        assert_eq!(failure(&b), Err(Error::Capture("unplugged".into())));
        wait_until("the source to be dropped", || counts.dropped.load(Ordering::SeqCst) == 1);
        assert!(hub.active().is_empty());
        assert!(matches!(a.recv_timeout(Duration::from_millis(10)), Err(Error::Capture(_))));
    }

    #[test]
    fn an_open_failure_is_returned_to_the_subscriber() {
        let (hub, counts) = hub(None);
        let err = hub.subscribe(&parse("image:/nonexistent/simee-camera/x.png"), FrameSpec::default()).err();
        assert!(matches!(&err, Some(Error::Io(m)) if m.contains("x.png")), "{err:?}");
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        assert!(hub.active().is_empty());
    }

    #[test]
    fn the_capture_runs_at_the_largest_size_and_highest_rate() {
        let subs: Vec<Subscriber> = [spec(320, 240, 60), spec(1280, 720, 15), spec(640, 480, 30)]
            .into_iter()
            .enumerate()
            .map(|(i, spec)| Subscriber { id: i as u64, spec, tx: mpsc::sync_channel(1).0, due: Instant::now() })
            .collect();
        assert_eq!(capture_spec(&subs), Some(spec(1280, 720, 60)));
        assert_eq!(capture_spec(&[]), None);
    }

    /// Host cameras and signals the test controls. `watch` hands out a fresh signal channel and
    /// counts the watches started and the ones dropped.
    #[derive(Clone, Default)]
    struct Plug {
        cameras: Arc<Mutex<Vec<HostCamera>>>,
        listings: Arc<AtomicUsize>,
        watches: Arc<AtomicUsize>,
        stopped: Arc<AtomicUsize>,
        signal: Arc<Mutex<Option<mpsc::Sender<Result<()>>>>>,
        refuse: bool,
    }

    struct PlugSignals {
        rx: Receiver<Result<()>>,
        stopped: Arc<AtomicUsize>,
    }

    impl Signals for PlugSignals {
        fn wait(&mut self, timeout: Duration) -> Result<bool> {
            match self.rx.recv_timeout(timeout) {
                Ok(Ok(())) => Ok(true),
                Ok(Err(e)) => Err(e),
                Err(_) => Ok(false),
            }
        }
    }

    impl Drop for PlugSignals {
        fn drop(&mut self) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl HotPlug for Plug {
        fn cameras(&self) -> Result<Vec<HostCamera>> {
            self.listings.fetch_add(1, Ordering::SeqCst);
            Ok(lock(&self.cameras).clone())
        }

        fn watch(&self) -> Result<Box<dyn Signals>> {
            if self.refuse {
                return Err(Error::Unsupported("no hot-plug here".into()));
            }
            self.watches.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel();
            *lock(&self.signal) = Some(tx);
            Ok(Box::new(PlugSignals { rx, stopped: self.stopped.clone() }))
        }
    }

    impl Plug {
        fn signal(&self, what: Result<()>) {
            if let Some(tx) = lock(&self.signal).as_ref() {
                let _ = tx.send(what);
            }
        }

        fn set(&self, cameras: &[HostCamera]) {
            *lock(&self.cameras) = cameras.to_vec();
        }
    }

    fn plugged(fail_after: Option<usize>) -> (Hub, Plug) {
        let plug = Plug::default();
        (Hub::with_hotplug(Fake { counts: Counts::default(), fail_after }, plug.clone()), plug)
    }

    fn next_event(events: &Events) -> Event {
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(e)) => e,
            other => panic!("no event: {other:?}"),
        }
    }

    #[test]
    fn a_burst_of_hot_plug_signals_is_one_listing_and_its_changes() {
        let (hub, plug) = plugged(None);
        let (a, b) = (crate::catalog::tests::camera("a", "A"), crate::catalog::tests::camera("b", "B"));
        plug.set(std::slice::from_ref(&a));
        let events = hub.events().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(plug.listings.load(Ordering::SeqCst), 1, "the baseline");
        plug.set(&[a.clone(), b.clone()]);
        for _ in 0..3 {
            plug.signal(Ok(()));
        }
        assert_eq!(next_event(&events), Event::Added(b.clone()));
        assert_eq!(events.recv_timeout(QUIET * 2), Ok(None), "one listing, one change");
        assert_eq!(plug.listings.load(Ordering::SeqCst), 2);

        let second = hub.events().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(plug.watches.load(Ordering::SeqCst), 1, "streams share one watch");
        plug.set(std::slice::from_ref(&b));
        plug.signal(Ok(()));
        assert_eq!(next_event(&events), Event::Removed(a.clone()));
        assert_eq!(next_event(&second), Event::Removed(a));
    }

    #[test]
    fn the_watch_stops_with_the_last_stream_and_restarts_on_demand() {
        let (hub, plug) = plugged(None);
        let (one, two) = (hub.events().unwrap_or_else(|e| panic!("{e}")), hub.events().unwrap_or_else(|e| panic!("{e}")));
        drop(one);
        thread::sleep(IDLE * 2);
        assert_eq!(plug.stopped.load(Ordering::SeqCst), 0, "one stream left");
        drop(two);
        wait_until("the watch to stop", || plug.stopped.load(Ordering::SeqCst) == 1);
        let _three = hub.events().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(plug.watches.load(Ordering::SeqCst), 2, "a fresh watch");
    }

    #[test]
    fn an_unplugged_source_is_reported_missing_to_its_subscribers_and_the_event_stream() {
        let (hub, _plug) = plugged(Some(2));
        let events = hub.events().unwrap_or_else(|e| panic!("{e}"));
        let sub = hub.subscribe(&parse("webcam:cam"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        let failure = loop {
            match sub.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(_)) => continue,
                other => break other,
            }
        };
        let missing = Error::Missing("the camera was unplugged".into());
        assert_eq!(failure, Err(missing.clone()));
        assert_eq!(next_event(&events), Event::SourceMissing { source: "webcam:the-id".into(), error: missing });
        // Another failure isn't a missing source.
        let other = hub.subscribe(&parse("solid:red"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        while let Ok(Some(_)) = other.recv_timeout(Duration::from_secs(5)) {}
        assert_eq!(events.recv_timeout(Duration::from_millis(100)), Ok(None));
    }

    #[test]
    fn a_watch_that_cannot_start_or_fails_ends_its_streams_with_the_reason() {
        let plug = Plug { refuse: true, ..Plug::default() };
        let hub = Hub::with_hotplug(Fake { counts: Counts::default(), fail_after: None }, plug);
        assert!(matches!(hub.events().err(), Some(Error::Unsupported(_))));

        let (hub, plug) = plugged(None);
        let events = hub.events().unwrap_or_else(|e| panic!("{e}"));
        plug.signal(Err(Error::Io("inotify: read failed".into())));
        let err = loop {
            match events.recv_timeout(Duration::from_secs(5)) {
                Ok(None) => panic!("the failure never came"),
                Ok(Some(_)) => continue,
                Err(e) => break e,
            }
        };
        assert_eq!(err, Error::Io("inotify: read failed".into()));
        // The next stream starts a fresh watch.
        assert!(hub.events().is_ok());
        assert_eq!(plug.watches.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_restarted_watch_keeps_the_cause_for_streams_that_had_not_read_it() {
        let (hub, plug) = plugged(None);
        let first = hub.events().unwrap_or_else(|e| panic!("{e}"));
        plug.signal(Err(Error::Io("inotify: read failed".into())));
        wait_until("the watch to fail", || !lock(&hub.inner.watch).running);
        // Another caller restarts the watch before `first` reads the failure.
        let second = hub.events().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(plug.watches.load(Ordering::SeqCst), 2);
        assert_eq!(first.try_recv(), Err(Error::Io("inotify: read failed".into())));
        assert_eq!(second.try_recv(), Ok(None));
    }
}
