//! Streaming capture from a V4L2-style device, with the ioctls behind [`Device`] so the loop runs
//! against a fake in tests: pick the mode nearest the request, set the format (`S_FMT`, then
//! `G_FMT` for what the driver chose) and the frame interval (`S_PARM`), request mmap buffers,
//! queue them all, `STREAMON`, then a capture thread polls, dequeues the newest frame, copies it
//! out and queues the buffer again at once. Frames are converted to BGRA only when the hub asks
//! for one, so a camera running faster than its subscribers costs a copy, not a decode, per frame.
//! Dropping the [`Stream`] stops the thread (within one poll slice), `STREAMOFF`s and frees the
//! buffers.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::convert::{self, Pixel};
use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::select::{self, Interval, Mode};
use crate::source::Source;
use crate::tcc::Authorization;

/// mmap buffers to ask for: one being filled, one being read, two spare.
pub(crate) const BUFFERS: u32 = 4;
/// How long the capture thread waits in one poll before checking whether it should stop.
const SLICE: Duration = Duration::from_millis(50);
/// How long a capture waits for its first frame, or tolerates no new one, before it reports why.
pub(crate) const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

// Linux errno values (the one host with a real `Device`).
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const ENXIO: i32 = 6;
const EACCES: i32 = 13;
const EBUSY: i32 = 16;
const ENODEV: i32 = 19;

/// One mode a device offers: its fourcc and its size and rates.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Offer {
    pub fourcc: u32,
    pub mode: Mode,
}

/// The format the driver settled on (`G_FMT` after `S_FMT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Negotiated {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes per row (`bytesperline`); 0 for compressed formats.
    pub stride: u32,
}

/// A buffer `DQBUF` handed back: which one, how many bytes the driver filled, and whether it
/// flagged the frame as damaged (`V4L2_BUF_FLAG_ERROR`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dequeued {
    pub index: u32,
    pub used: usize,
    pub damaged: bool,
}

/// The ioctls a streaming capture makes, on one open device node. Errors are the ioctl's `errno`
/// as an `io::Error`; [`classify`] turns them into the crate's errors.
pub(crate) trait Device: Send + 'static {
    /// Every format, size and interval the device offers (`ENUM_FMT`, `ENUM_FRAMESIZES`,
    /// `ENUM_FRAMEINTERVALS`). A stepwise size range offers its largest size and `want`'s size
    /// fitted into the range.
    fn offers(&mut self, want: FrameSpec) -> io::Result<Vec<Offer>>;
    /// `S_FMT` then `G_FMT`.
    fn set_format(&mut self, fourcc: u32, width: u32, height: u32) -> io::Result<Negotiated>;
    /// `S_PARM`'s time per frame.
    fn set_interval(&mut self, interval: Interval) -> io::Result<()>;
    /// `REQBUFS` with mmap memory; returns how many buffers the driver allocated.
    fn request_buffers(&mut self, count: u32) -> io::Result<u32>;
    /// `QUERYBUF` and mmap buffer `index`.
    fn map(&mut self, index: u32) -> io::Result<()>;
    /// `QBUF`.
    fn queue(&mut self, index: u32) -> io::Result<()>;
    fn stream_on(&mut self) -> io::Result<()>;
    fn stream_off(&mut self) -> io::Result<()>;
    /// `poll` for a filled buffer: `Ok(false)` on a timeout.
    fn wait(&mut self, timeout: Duration) -> io::Result<bool>;
    /// `DQBUF`; `Ok(None)` when no buffer is ready (`EAGAIN`).
    fn dequeue(&mut self) -> io::Result<Option<Dequeued>>;
    /// The first `len` bytes of mapped buffer `index`.
    fn data(&self, index: u32, len: usize) -> &[u8];
}

/// The crate's error for a failed device call: busy, permission, gone, or a capture failure that
/// names the call.
pub(crate) fn classify(err: &io::Error, what: &str, name: &str) -> Error {
    match err.raw_os_error() {
        Some(EBUSY) => Error::Busy(format!("{name} is in use by another program ({what}: {err}); close it and try again")),
        Some(EACCES | EPERM) => Error::Permission {
            status: Authorization::Denied,
            fix: format!(
                "{name} can't be opened by this user ({what}: {err}): add it to the device's group \
                 (usually `sudo usermod -aG video $USER`, then log in again)"
            ),
        },
        Some(ENODEV | ENXIO | ENOENT) => Error::Missing(format!("{name} was unplugged ({what}: {err})")),
        _ => Error::Capture(format!("{what} on {name}: {err}")),
    }
}

/// The newest frame the capture thread copied out, and why it stopped if it did.
#[derive(Default)]
struct Shared {
    state: Mutex<Latest>,
    arrived: Condvar,
    stop: AtomicBool,
}

#[derive(Default)]
struct Latest {
    raw: Option<Raw>,
    failure: Option<Error>,
}

#[derive(Clone)]
struct Raw {
    seq: u64,
    bytes: Arc<[u8]>,
    at: Instant,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The last frame converted, and when its raw frame arrived.
struct Converted {
    width: u32,
    height: u32,
    bgra: Arc<[u8]>,
    at: Instant,
}

/// A camera streaming through a [`Device`], as a [`Source`].
pub(crate) struct Stream {
    name: String,
    format: Negotiated,
    pixel: Pixel,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The newest raw frame taken (converted or found damaged).
    seen: u64,
    converted: Option<Converted>,
    /// Why the newest frame taken didn't convert.
    undecodable: Option<Error>,
}

impl Stream {
    /// Starts streaming from `device` in the mode nearest `want`.
    pub(crate) fn open<D: Device>(mut device: D, name: &str, want: FrameSpec) -> Result<Stream> {
        let offers = device.offers(want).map_err(|e| classify(&e, "listing formats", name))?;
        let usable: Vec<(u32, Mode)> = offers
            .iter()
            .filter_map(|o| Pixel::from_fourcc(o.fourcc).map(|p| (o.fourcc, Mode { rank: p.rank(), ..o.mode.clone() })))
            .collect();
        let modes: Vec<Mode> = usable.iter().map(|(_, m)| m.clone()).collect();
        let Some(choice) = select::nearest(&modes, want) else {
            let mut offered: Vec<String> = offers.iter().map(|o| convert::fourcc_text(o.fourcc)).collect();
            offered.dedup();
            return Err(Error::Unsupported(format!(
                "{name} offers none of the formats simee converts (YUYV, MJPEG); it offers {}",
                if offered.is_empty() { "none".to_string() } else { offered.join(", ") }
            )));
        };
        let (fourcc, mode) = &usable[choice.index];
        // YUYV packs two pixels per four bytes, so ask for an even width (a continuous size range
        // can offer an odd one).
        let width = if *fourcc == convert::YUYV { (mode.width & !1).max(2) } else { mode.width };
        let format = device.set_format(*fourcc, width, mode.height).map_err(|e| classify(&e, "VIDIOC_S_FMT", name))?;
        let pixel = Pixel::from_fourcc(format.fourcc).ok_or_else(|| {
            Error::Unsupported(format!("{name} switched to {} instead of the format asked for", convert::fourcc_text(format.fourcc)))
        })?;
        if pixel == Pixel::Yuyv && (format.width == 0 || format.height == 0 || (format.stride as usize) < format.width as usize * 2) {
            return Err(Error::Capture(format!(
                "{name} reported a {}x{} YUYV format with {}-byte rows",
                format.width, format.height, format.stride
            )));
        }
        if pixel == Pixel::Yuyv && format.width % 2 != 0 {
            return Err(Error::Unsupported(format!(
                "{name} settled on a {}-pixel-wide YUYV format; YUYV needs an even width",
                format.width
            )));
        }
        // Best effort: a driver without S_PARM runs at its own rate.
        if let Some(interval) = choice.interval {
            let _ = device.set_interval(interval);
        }
        let count = device.request_buffers(BUFFERS).map_err(|e| classify(&e, "VIDIOC_REQBUFS", name))?;
        if count < 2 {
            return Err(Error::Capture(format!("{name} gave {count} capture buffer(s); streaming needs at least 2")));
        }
        for index in 0..count {
            device.map(index).map_err(|e| classify(&e, "mmap", name))?;
            device.queue(index).map_err(|e| classify(&e, "VIDIOC_QBUF", name))?;
        }
        device.stream_on().map_err(|e| classify(&e, "VIDIOC_STREAMON", name))?;
        let shared = Arc::new(Shared::default());
        let (run, label) = (shared.clone(), name.to_string());
        let thread = thread::Builder::new()
            .name("camera v4l2".into())
            .spawn(move || capture(device, &label, &run))
            .map_err(|e| Error::Capture(format!("can't start the capture thread: {e}")))?;
        Ok(Stream { name: name.to_string(), format, pixel, shared, thread: Some(thread), seen: 0, converted: None, undecodable: None })
    }
}

/// The capture thread: poll, take the newest filled buffer (requeuing older ones unread), copy it
/// out, requeue it; until told to stop or the device fails.
fn capture<D: Device>(mut device: D, name: &str, shared: &Shared) {
    let mut seq = 0u64;
    let failure = 'run: loop {
        if shared.stop.load(Ordering::Acquire) {
            break None;
        }
        match device.wait(SLICE) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => break Some(classify(&e, "poll", name)),
        }
        let mut newest: Option<Dequeued> = None;
        loop {
            match device.dequeue() {
                Ok(None) => break,
                Ok(Some(buf)) if buf.damaged || buf.used == 0 => {
                    if let Err(e) = device.queue(buf.index) {
                        break 'run Some(classify(&e, "VIDIOC_QBUF", name));
                    }
                }
                Ok(Some(buf)) => {
                    if let Some(older) = newest.replace(buf)
                        && let Err(e) = device.queue(older.index)
                    {
                        break 'run Some(classify(&e, "VIDIOC_QBUF", name));
                    }
                }
                Err(e) => break 'run Some(classify(&e, "VIDIOC_DQBUF", name)),
            }
        }
        let Some(buf) = newest else { continue };
        let bytes: Arc<[u8]> = device.data(buf.index, buf.used).into();
        if let Err(e) = device.queue(buf.index) {
            break Some(classify(&e, "VIDIOC_QBUF", name));
        }
        seq += 1;
        lock(&shared.state).raw = Some(Raw { seq, bytes, at: Instant::now() });
        shared.arrived.notify_all();
    };
    let _ = device.stream_off();
    drop(device);
    if let Some(e) = failure {
        lock(&shared.state).failure = Some(e);
        shared.arrived.notify_all();
    }
}

impl Source for Stream {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        loop {
            let raw = {
                let mut state = lock(&self.shared.state);
                loop {
                    if let Some(e) = &state.failure {
                        return Err(e.clone());
                    }
                    if let Some(raw) = state.raw.as_ref().filter(|r| r.seq > self.seen) {
                        break raw.clone();
                    }
                    // Nothing new: the last frame again, unless the camera has gone quiet or
                    // nothing it sent has decoded for a while.
                    if let Some(c) = &self.converted {
                        if state.raw.as_ref().map_or(c.at, |r| r.at).elapsed() > FRAME_TIMEOUT {
                            return Err(stalled(&self.name));
                        }
                        if c.at.elapsed() > FRAME_TIMEOUT {
                            return Err(self.undecodable.clone().unwrap_or_else(|| stalled(&self.name)));
                        }
                        return Ok(
                            CameraFrame { width: c.width, height: c.height, bgra: c.bgra.clone(), tick }.fit(spec.width, spec.height)
                        );
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(self.undecodable.clone().unwrap_or_else(|| stalled(&self.name)));
                    }
                    state = self.shared.arrived.wait_timeout(state, deadline - now).map(|(s, _)| s).unwrap_or_else(|p| p.into_inner().0);
                }
            };
            self.seen = raw.seq;
            let f = self.format;
            match convert::to_bgra(self.pixel, f.width, f.height, f.stride as usize, &raw.bytes) {
                Ok((width, height, bgra)) => {
                    let bgra: Arc<[u8]> = bgra.into();
                    self.undecodable = None;
                    self.converted = Some(Converted { width, height, bgra: bgra.clone(), at: raw.at });
                    return Ok(CameraFrame { width, height, bgra, tick }.fit(spec.width, spec.height));
                }
                // A damaged frame: skipped for the next one, or the last good one.
                Err(e) => self.undecodable = Some(e),
            }
        }
    }
}

fn stalled(name: &str) -> Error {
    Error::Capture(format!("no frames from {name} for {} s", FRAME_TIMEOUT.as_secs()))
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::convert::tests::{fixture_jpeg, without_huffman_tables};
    use crate::convert::{MJPG, YUYV};
    use crate::select::tests::mode;

    pub(crate) enum Step {
        Frame(Vec<u8>),
        Damaged,
        Fail(i32),
    }

    /// What the fake was asked to do, and the frames still to come (tests push them while it runs).
    #[derive(Default)]
    pub(crate) struct Script {
        pub calls: Vec<String>,
        pub steps: VecDeque<Step>,
    }

    pub(crate) struct Fake {
        offers: Vec<Offer>,
        /// What `S_FMT` settles on; `None`: what was asked for.
        negotiate: Option<Negotiated>,
        /// A call that fails, and its errno.
        fail: Option<(&'static str, i32)>,
        granted: u32,
        script: Arc<Mutex<Script>>,
        buffers: Vec<Vec<u8>>,
        queued: VecDeque<u32>,
    }

    impl Fake {
        pub(crate) fn new(offers: Vec<Offer>) -> (Fake, Arc<Mutex<Script>>) {
            let script = Arc::new(Mutex::new(Script::default()));
            let fake = Fake {
                offers,
                negotiate: None,
                fail: None,
                granted: BUFFERS,
                script: script.clone(),
                buffers: Vec::new(),
                queued: VecDeque::new(),
            };
            (fake, script)
        }

        fn call(&mut self, name: &'static str, detail: String) -> io::Result<()> {
            lock(&self.script).calls.push(if detail.is_empty() { name.to_string() } else { format!("{name} {detail}") });
            match self.fail {
                Some((n, errno)) if n == name => Err(io::Error::from_raw_os_error(errno)),
                _ => Ok(()),
            }
        }
    }

    impl Device for Fake {
        fn offers(&mut self, _want: FrameSpec) -> io::Result<Vec<Offer>> {
            self.call("ENUM", String::new())?;
            Ok(self.offers.clone())
        }

        fn set_format(&mut self, fourcc: u32, width: u32, height: u32) -> io::Result<Negotiated> {
            self.call("S_FMT", format!("{} {width}x{height}", convert::fourcc_text(fourcc)))?;
            let stride = if fourcc == YUYV { width * 2 } else { 0 };
            Ok(self.negotiate.unwrap_or(Negotiated { fourcc, width, height, stride }))
        }

        fn set_interval(&mut self, interval: Interval) -> io::Result<()> {
            self.call("S_PARM", format!("{}/{}", interval.num, interval.den))
        }

        fn request_buffers(&mut self, count: u32) -> io::Result<u32> {
            self.call("REQBUFS", count.to_string())?;
            Ok(self.granted.min(count))
        }

        fn map(&mut self, index: u32) -> io::Result<()> {
            self.call("MAP", index.to_string())?;
            self.buffers.push(vec![0; 1 << 20]);
            Ok(())
        }

        fn queue(&mut self, index: u32) -> io::Result<()> {
            if !self.queued.contains(&index) {
                self.queued.push_back(index);
            }
            match self.fail {
                Some(("QBUF", errno)) => Err(io::Error::from_raw_os_error(errno)),
                _ => Ok(()),
            }
        }

        fn stream_on(&mut self) -> io::Result<()> {
            self.call("STREAMON", String::new())
        }

        fn stream_off(&mut self) -> io::Result<()> {
            self.call("STREAMOFF", String::new())
        }

        fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
            if lock(&self.script).steps.is_empty() {
                thread::sleep(timeout);
                return Ok(false);
            }
            Ok(true)
        }

        fn dequeue(&mut self) -> io::Result<Option<Dequeued>> {
            let step = lock(&self.script).steps.pop_front();
            let Some(step) = step else { return Ok(None) };
            let Some(index) = self.queued.pop_front() else { return Ok(None) };
            match step {
                Step::Frame(bytes) => {
                    self.buffers[index as usize][..bytes.len()].copy_from_slice(&bytes);
                    Ok(Some(Dequeued { index, used: bytes.len(), damaged: false }))
                }
                Step::Damaged => Ok(Some(Dequeued { index, used: 1, damaged: true })),
                Step::Fail(errno) => Err(io::Error::from_raw_os_error(errno)),
            }
        }

        fn data(&self, index: u32, len: usize) -> &[u8] {
            &self.buffers[index as usize][..len]
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            lock(&self.script).calls.push("CLOSE".into());
        }
    }

    pub(crate) fn offer(fourcc: u32, width: u32, height: u32, rates: &[u64]) -> Offer {
        Offer { fourcc, mode: mode(width, height, 0, rates) }
    }

    fn push(script: &Arc<Mutex<Script>>, step: Step) {
        lock(script).steps.push_back(step);
    }

    fn calls(script: &Arc<Mutex<Script>>) -> Vec<String> {
        lock(script).calls.clone()
    }

    /// `width`x`height` YUYV of one luma, neutral chroma.
    pub(crate) fn yuyv(width: u32, height: u32, luma: u8) -> Vec<u8> {
        [luma, 128].repeat(width as usize * height as usize)
    }

    fn spec(w: u32, h: u32, fps: u32) -> FrameSpec {
        FrameSpec { width: w, height: h, fps }
    }

    fn webcam_offers() -> Vec<Offer> {
        vec![
            offer(YUYV, 640, 480, &[30, 15]),
            offer(YUYV, 1280, 720, &[10, 5]),
            offer(MJPG, 1280, 720, &[30, 15]),
            offer(u32::from_le_bytes(*b"H264"), 1920, 1080, &[30]),
        ]
    }

    #[test]
    fn streams_yuyv_in_the_nearest_mode_and_cleans_up_on_drop() {
        let (fake, script) = Fake::new(webcam_offers());
        let mut stream = Stream::open(fake, "Cam", spec(640, 480, 30)).unwrap_or_else(|e| panic!("{e}"));
        push(&script, Step::Frame(yuyv(640, 480, 235)));
        let frame = stream.frame(spec(640, 480, 30), Tick::start()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((frame.width, frame.height), (640, 480));
        assert_eq!(frame.pixel(320, 240), [255, 255, 255, 255]);
        // The same frame again until a new one comes, without converting it twice.
        let again = stream.frame(spec(640, 480, 30), Tick::start()).unwrap_or_else(|e| panic!("{e}"));
        assert!(Arc::ptr_eq(&frame.bgra, &again.bgra));
        push(&script, Step::Frame(yuyv(640, 480, 16)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while stream.frame(spec(640, 480, 30), Tick::start()).map(|f| f.pixel(0, 0)) != Ok([0, 0, 0, 255]) {
            assert!(Instant::now() < deadline, "the second frame never arrived");
            thread::sleep(Duration::from_millis(5));
        }
        drop(stream);
        let expected = [
            "ENUM",
            "S_FMT YUYV 640x480",
            "S_PARM 1/30",
            "REQBUFS 4",
            "MAP 0",
            "MAP 1",
            "MAP 2",
            "MAP 3",
            "STREAMON",
            "STREAMOFF",
            "CLOSE",
        ];
        assert_eq!(calls(&script), expected);
    }

    #[test]
    fn the_rate_picks_mjpeg_over_a_slow_yuyv_at_the_same_size() {
        let (fake, script) = Fake::new(webcam_offers());
        let stream = Stream::open(fake, "Cam", spec(1280, 720, 30)).unwrap_or_else(|e| panic!("{e}"));
        drop(stream);
        let calls = calls(&script);
        assert_eq!(calls[1..3], ["S_FMT MJPG 1280x720", "S_PARM 1/30"]);
    }

    #[test]
    fn mjpeg_frames_decode_and_damaged_ones_are_skipped() {
        let (fake, script) = Fake::new(vec![offer(MJPG, 16, 16, &[30])]);
        let mut stream = Stream::open(fake, "Cam", spec(16, 16, 30)).unwrap_or_else(|e| panic!("{e}"));
        push(&script, Step::Damaged);
        push(&script, Step::Frame(b"garbage".to_vec()));
        // Let the capture thread publish the garbage frame before the good one comes.
        thread::sleep(Duration::from_millis(150));
        push(&script, Step::Frame(without_huffman_tables(&fixture_jpeg())));
        let deadline = Instant::now() + Duration::from_secs(5);
        let red = loop {
            if let Ok(f) = stream.frame(spec(16, 16, 30), Tick::start()) {
                break f;
            }
            assert!(Instant::now() < deadline, "the good frame never decoded");
        };
        let [b, g, r, _] = red.pixel(8, 8);
        assert!(r > 230 && g < 30 && b < 30, "{r},{g},{b}");
        // A broken frame after a good one: the good one again.
        push(&script, Step::Frame(b"\xff\xd8broken".to_vec()));
        thread::sleep(Duration::from_millis(150));
        let still = stream.frame(spec(16, 16, 30), Tick::start()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(still.bgra, red.bgra);
    }

    #[test]
    fn frames_follow_the_format_the_driver_settled_on() {
        let (mut fake, script) = Fake::new(vec![offer(YUYV, 640, 480, &[30])]);
        fake.negotiate = Some(Negotiated { fourcc: YUYV, width: 320, height: 240, stride: 768 });
        let mut stream = Stream::open(fake, "Cam", spec(640, 480, 30)).unwrap_or_else(|e| panic!("{e}"));
        // 240 rows of 768 bytes (640 of pixels, 128 of padding).
        let row: Vec<u8> = [235u8, 128].repeat(320).into_iter().chain([0u8; 128]).collect();
        push(&script, Step::Frame(row.repeat(240)));
        let frame = stream.frame(spec(320, 240, 30), Tick::start()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((frame.width, frame.height, frame.pixel(319, 239)), (320, 240, [255, 255, 255, 255]));
    }

    #[test]
    fn yuyv_is_requested_at_an_even_width_and_an_odd_negotiated_width_is_refused() {
        let (fake, script) = Fake::new(vec![offer(YUYV, 641, 480, &[30])]);
        assert!(Stream::open(fake, "Cam", spec(641, 480, 30)).is_ok());
        assert!(calls(&script).contains(&"S_FMT YUYV 640x480".to_string()), "{:?}", calls(&script));

        let (mut fake, _) = Fake::new(vec![offer(YUYV, 640, 480, &[30])]);
        fake.negotiate = Some(Negotiated { fourcc: YUYV, width: 639, height: 480, stride: 1278 });
        let err = Stream::open(fake, "Cam", spec(640, 480, 30)).err();
        assert!(matches!(&err, Some(Error::Unsupported(m)) if m.contains("even width")), "{err:?}");
    }

    #[test]
    fn an_unplug_mid_stream_is_source_missing() {
        let (fake, script) = Fake::new(vec![offer(YUYV, 640, 480, &[30])]);
        let mut stream = Stream::open(fake, "Cam", spec(640, 480, 30)).unwrap_or_else(|e| panic!("{e}"));
        push(&script, Step::Frame(yuyv(640, 480, 128)));
        assert!(stream.frame(spec(640, 480, 30), Tick::start()).is_ok());
        push(&script, Step::Fail(ENODEV));
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = loop {
            match stream.frame(spec(640, 480, 30), Tick::start()) {
                Err(e) => break e,
                Ok(_) => assert!(Instant::now() < deadline, "the unplug was never reported"),
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(&err, Error::Missing(m) if m.contains("Cam") && m.contains("VIDIOC_DQBUF")), "{err:?}");
        assert!(calls(&script).ends_with(&["STREAMOFF".to_string(), "CLOSE".to_string()]), "stopped and closed");
    }

    #[test]
    fn busy_permission_and_other_failures_say_which() {
        type Check = fn(&Error) -> bool;
        let cases: [(&str, i32, Check); 5] = [
            ("S_FMT", EBUSY, |e| matches!(e, Error::Busy(m) if m.contains("in use"))),
            ("STREAMON", EACCES, |e| matches!(e, Error::Permission { fix, .. } if fix.contains("video"))),
            ("REQBUFS", 5, |e| matches!(e, Error::Capture(m) if m.contains("VIDIOC_REQBUFS"))),
            ("ENUM", ENODEV, |e| matches!(e, Error::Missing(_))),
            ("MAP", 12, |e| matches!(e, Error::Capture(m) if m.contains("mmap"))),
        ];
        for (call, errno, ok) in cases {
            let (mut fake, script) = Fake::new(vec![offer(YUYV, 640, 480, &[30])]);
            fake.fail = Some((call, errno));
            let err = Stream::open(fake, "Cam", FrameSpec::default()).err();
            assert!(err.as_ref().is_some_and(ok), "{call} errno {errno}: {err:?}");
            assert_eq!(calls(&script).last().map(String::as_str), Some("CLOSE"), "{call}: the node is closed");
        }
        // A failing S_PARM isn't fatal: the camera runs at its own rate.
        let (mut fake, _) = Fake::new(vec![offer(YUYV, 640, 480, &[30])]);
        fake.fail = Some(("S_PARM", 22));
        assert!(Stream::open(fake, "Cam", FrameSpec::default()).is_ok());
    }

    #[test]
    fn unconvertible_cameras_and_short_buffer_grants_are_refused() {
        let (fake, _) =
            Fake::new(vec![offer(u32::from_le_bytes(*b"H264"), 1920, 1080, &[30]), offer(u32::from_le_bytes(*b"NV12"), 640, 480, &[30])]);
        let err = Stream::open(fake, "Cam", FrameSpec::default()).err();
        assert!(matches!(&err, Some(Error::Unsupported(m)) if m.contains("H264, NV12")), "{err:?}");
        let (mut fake, _) = Fake::new(vec![offer(YUYV, 640, 480, &[30])]);
        fake.granted = 1;
        let err = Stream::open(fake, "Cam", FrameSpec::default()).err();
        assert!(matches!(&err, Some(Error::Capture(m)) if m.contains("1 capture buffer")), "{err:?}");
        let (mut fake, _) = Fake::new(vec![offer(YUYV, 640, 480, &[30])]);
        fake.negotiate = Some(Negotiated { fourcc: u32::from_le_bytes(*b"NV12"), width: 640, height: 480, stride: 640 });
        assert!(matches!(Stream::open(fake, "Cam", FrameSpec::default()), Err(Error::Unsupported(_))));
    }

    #[test]
    fn errnos_classify() {
        let e = |errno| classify(&io::Error::from_raw_os_error(errno), "open", "/dev/video0");
        assert!(matches!(e(ENOENT), Error::Missing(_)));
        assert!(matches!(e(ENXIO), Error::Missing(_)));
        assert!(matches!(e(EPERM), Error::Permission { status: Authorization::Denied, .. }));
        assert!(matches!(e(EBUSY), Error::Busy(m) if m.starts_with("/dev/video0")));
        assert!(matches!(e(5), Error::Capture(m) if m.starts_with("open on /dev/video0")));
    }
}
