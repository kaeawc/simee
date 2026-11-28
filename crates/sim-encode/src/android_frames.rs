//! An Android emulator's frames, newest first: [`FrameReader`] connects to `<avd>/simee/display.sock`
//! and keeps a reader thread that holds only the newest frame (the ring's RGBA rows, read once into
//! a buffer it gets back from the consumer), stamped with when it arrived. Each platform's
//! `AndroidSource` turns that frame into the picture its encoder takes.
//!
//! On macOS the reader also asks for frames as IOSurfaces (`surfaces <name>`, the port of a
//! [`sim_coresim::SurfaceInbox`]): an emulator that has them (`emu -gpu metal`) sends each frame's
//! surface there, and the frame carries it ([`Taken::surface`]) instead of pixels. The surface is
//! the emulator's until released: dropping the [`HeldSurface`] sends `release <seq>`. Any frame
//! that isn't a surface still comes as pixels.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use sim_android::display::{Display as AndroidDisplay, Frames, Next};
use sim_core::stream::Input;

use crate::{Error, Result};

/// The newest frame the reader thread has, not yet taken.
#[derive(Default)]
struct Mailbox {
    frame: Option<Frame>,
    /// A buffer to read the next frame into (the last one taken).
    spare: Vec<u8>,
    closed: Option<String>,
    device: (u32, u32),
}

struct Frame {
    seq: u64,
    at: Instant,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    surface: Option<HeldSurface>,
}

/// An emulator's IOSurface for one frame, held until this is dropped, which tells the emulator
/// (`release <seq>`) it may write the surface again.
#[cfg(target_os = "macos")]
pub struct HeldSurface {
    pub seq: u32,
    pub surface: sim_coresim::Surface,
    link: Arc<UnixStream>,
}

#[cfg(target_os = "macos")]
impl Drop for HeldSurface {
    fn drop(&mut self) {
        release(&self.link, self.seq);
    }
}

/// Tells the emulator it may have surface `seq` back. One short write: lines from other threads
/// (input) don't interleave with it.
#[cfg(target_os = "macos")]
fn release(link: &UnixStream, seq: u32) {
    use std::io::Write;
    let line = format!("{}\n", Input::Release(seq).to_line());
    let _ = (&*link).write_all(line.as_bytes());
}

/// Asks the emulator for pixels instead of surfaces from the next frame on (`surfaces -`).
#[cfg(target_os = "macos")]
fn surfaces_off(link: &UnixStream) {
    use std::io::Write;
    let line = format!("{}\n", Input::Surfaces(sim_core::stream::NO_SURFACES.into()).to_line());
    let _ = (&*link).write_all(line.as_bytes());
}

/// Whether `surface` is the frame its record describes: the record's size, in BGRA (what the
/// encoder wraps it as).
#[cfg(target_os = "macos")]
fn fits(surface: &sim_coresim::Surface, header: &sim_core::stream::FrameHeader) -> bool {
    surface.size() == (header.width, header.height) && surface.pixel_format() == *b"BGRA"
}

/// No surfaces off macOS.
#[cfg(not(target_os = "macos"))]
pub enum HeldSurface {}

/// Waits for surface `seq` in `inbox`: its message goes out just before its record, so it's
/// there (or one before it whose record was passed over, released here).
#[cfg(target_os = "macos")]
fn receive(inbox: &sim_coresim::SurfaceInbox, seq: u32, link: &Arc<UnixStream>) -> Option<HeldSurface> {
    const WAIT: std::time::Duration = std::time::Duration::from_millis(500);
    loop {
        let (got, surface) = inbox.recv(WAIT).ok()??;
        let held = HeldSurface { seq: got, surface, link: link.clone() };
        if got == seq {
            return Some(held);
        }
        // An older one (dropped: released); a newer one can't come before its record.
        if got > seq {
            return None;
        }
    }
}

#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A frame taken from the mailbox.
pub struct Taken {
    /// When the reader thread had it.
    pub at: Instant,
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes of RGBA, rows tightly packed (empty for a surface). Give it
    /// back with [`FrameReader::recycle`] when done: the next frame is read into it.
    pub rgba: Vec<u8>,
    /// The frame as the emulator's IOSurface (BGRA, `width` x `height`), when it sent one (never
    /// off macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub surface: Option<HeldSurface>,
    /// Frames that arrived and were replaced by this one without [`FrameReader::wait`] ever
    /// reporting them.
    pub unreported: u64,
}

/// A running AVD's display, frames through the shared ring.
pub struct FrameReader {
    shared: Arc<Shared>,
    /// The connection's write side: `hello`, and input from [`FrameReader::input`].
    link: UnixStream,
    /// The last frame `wait` reported, by sequence number.
    reported: Option<u64>,
    reader: Option<JoinHandle<()>>,
}

impl FrameReader {
    /// Connects to the AVD in `avd_dir` and asks for frames at most `max_width` pixels wide (0:
    /// full size; emu shrinks by a whole factor, `ceil(width / max_width)`).
    pub fn connect(avd_dir: &Path, max_width: u32) -> Result<FrameReader> {
        let source = |e: String| Error::Source(e);
        let mut display = AndroidDisplay::connect(avd_dir).map_err(source)?;
        display.request_shared().map_err(source)?;
        // Surfaces where the emulator has them; an emulator without ignores the line. Only the
        // emulator at the other end of this connection may send them.
        #[cfg(target_os = "macos")]
        let mut inbox = display
            .try_clone()
            .ok()
            .and_then(|s| sim_coresim::surface_port::peer_pid(&s))
            .and_then(|pid| sim_coresim::SurfaceInbox::check_in(pid).ok());
        #[cfg(target_os = "macos")]
        if let Some(inbox) = &inbox {
            display.send(&Input::Surfaces(inbox.name().to_owned())).map_err(source)?;
        }
        // `hello 0` pauses the stream (a hidden window's); full size is a width no frame exceeds.
        let max_width = if max_width == 0 { u32::MAX } else { max_width };
        display.send(&Input::Hello { max_width }).map_err(source)?;
        let mut frames = Frames::new(display.try_clone().map_err(source)?).with_fallback(display.try_clone().map_err(source)?);
        let link = display.try_clone().map_err(source)?;
        #[cfg(target_os = "macos")]
        let releases = Arc::new(display.try_clone().map_err(source)?);
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let reader = std::thread::Builder::new()
            .name("simee-encode-android".into())
            .spawn(move || {
                let mut buf = Vec::new();
                for seq in 0.. {
                    match frames.newest_into(&mut buf, || true) {
                        Ok(next) => {
                            let (h, rgba, surface) = match next {
                                Next::Pixels(h) => (h, std::mem::take(&mut buf), None),
                                #[cfg(target_os = "macos")]
                                Next::Surface { header, seq } => match inbox.as_ref().and_then(|i| receive(i, seq, &releases)) {
                                    Some(held) if fits(&held.surface, &header) => (header, Vec::new(), Some(held)),
                                    // Not the frame its record says: it goes back (dropped), and
                                    // frames come as pixels from now on.
                                    Some(held) => {
                                        eprintln!(
                                            "simee: a {:?} {:?} surface for a {}x{} frame; taking frames as pixels",
                                            held.surface.size(),
                                            String::from_utf8_lossy(&held.surface.pixel_format()),
                                            header.width,
                                            header.height
                                        );
                                        drop(held);
                                        surfaces_off(&releases);
                                        inbox = None;
                                        continue;
                                    }
                                    // Never came: let the emulator have it back.
                                    None => {
                                        release(&releases, seq);
                                        continue;
                                    }
                                },
                                #[cfg(not(target_os = "macos"))]
                                Next::Surface { .. } => continue,
                            };
                            let at = Instant::now();
                            let mut m = s.lock();
                            m.device = (h.device_width, h.device_height);
                            let frame = Frame { seq, at, width: h.width, height: h.height, rgba, surface };
                            // A surface nobody took is released as it's replaced.
                            let replaced = m.frame.replace(frame);
                            // Read the next frame into the one nobody took, or the spare (a surface
                            // frame left the buffer here).
                            if buf.capacity() == 0 {
                                buf = match replaced {
                                    Some(f) if f.rgba.capacity() > 0 => f.rgba,
                                    _ => std::mem::take(&mut m.spare),
                                };
                            }
                            drop(m);
                            s.changed.notify_one();
                        }
                        Err(e) => {
                            s.lock().closed = Some(e.to_string());
                            s.changed.notify_one();
                            return;
                        }
                    }
                }
            })
            .map_err(|e| Error::Source(e.to_string()))?;
        Ok(FrameReader { shared, link, reported: None, reader: Some(reader) })
    }

    /// The device's own display size (what input coordinates are in), once a frame has arrived.
    pub fn device_size(&self) -> Option<(u32, u32)> {
        Some(self.shared.lock().device).filter(|&(w, h)| w > 0 && h > 0)
    }

    /// A second write handle on the connection, for sending input lines from another thread.
    pub fn input_link(&self) -> Result<UnixStream> {
        self.link.try_clone().map_err(|e| Error::Source(e.to_string()))
    }

    /// Asks the emulator for frames as pixels from now on, not surfaces (one it sent couldn't be
    /// used).
    #[cfg(target_os = "macos")]
    pub fn stop_surfaces(&self) {
        surfaces_off(&self.link);
    }

    /// Sends one input line (a touch, a key) on the display connection.
    pub fn input(&mut self, input: &Input) -> Result<()> {
        use std::io::Write;
        let line = format!("{}\n", input.to_line());
        self.link.write_all(line.as_bytes()).map_err(|e| Error::Source(e.to_string()))
    }

    /// Sleeps until a frame not yet reported arrives or `deadline` passes; the frame's arrival, or
    /// `None` at the deadline. An error (the display connection closed) ends the stream.
    pub fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
        let shared = self.shared.clone();
        let mut m = shared.lock();
        loop {
            // Each frame is reported once; it stays in the mailbox until `take` takes it or a
            // newer one replaces it.
            if let Some(f) = &m.frame
                && self.reported != Some(f.seq)
            {
                self.reported = Some(f.seq);
                return Ok(Some(f.at));
            }
            if let Some(e) = &m.closed {
                return Err(Error::Source(format!("the display connection closed: {e}")));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            m = self.shared.changed.wait_timeout(m, deadline - now).map_or_else(|p| p.into_inner().0, |(g, _)| g);
        }
    }

    /// The frame in the mailbox, if one arrived since the last call. The reader may have replaced
    /// the frame `wait` reported with newer ones; those are counted in [`Taken::unreported`].
    pub fn take(&mut self) -> Option<Taken> {
        let frame = self.shared.lock().frame.take()?;
        let unreported = self.reported.map_or(0, |r| frame.seq.saturating_sub(r));
        self.reported = Some(self.reported.map_or(frame.seq, |r| r.max(frame.seq)));
        Some(Taken { at: frame.at, width: frame.width, height: frame.height, rgba: frame.rgba, surface: frame.surface, unreported })
    }

    /// The sequence number of the frame in the mailbox, and of the last one `wait` reported: what
    /// a test needs to wait for a frame to have arrived, and to say how many were skipped.
    #[cfg(test)]
    pub(crate) fn seqs(&self) -> (Option<u64>, Option<u64>) {
        (self.shared.lock().frame.as_ref().map(|f| f.seq), self.reported)
    }

    /// Hands a taken frame's buffer back for the reader to fill with the next one.
    pub fn recycle(&self, buffer: Vec<u8>) {
        self.shared.lock().spare = buffer;
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        // Closing both directions wakes the reader out of its blocking read.
        let _ = self.link.shutdown(std::net::Shutdown::Both);
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use sim_core::stream::FrameHeader;

    /// A display socket in a fresh AVD directory. Once a client has connected, it sends nothing
    /// until [`FakeDisplay::go`]; then it sends `frames` solid frames of `size` (frame `i` is
    /// filled with byte `i`), one every `gap`, on the socket itself (no ring), and keeps the
    /// connection open until it is dropped. Holding the frames back is what lets a test look at
    /// the source before any frame exists.
    pub(crate) struct FakeDisplay {
        pub(crate) dir: std::path::PathBuf,
        go: std::sync::mpsc::Sender<()>,
        _done: std::sync::mpsc::Sender<()>,
    }

    impl FakeDisplay {
        pub(crate) fn go(&self) {
            self.go.send(()).expect("the fake display is running");
        }
    }

    /// A fresh `/tmp/simee-<pid>-<n>-<name>` directory (not under TMPDIR, which can be long enough
    /// that a socket path inside it exceeds SUN_LEN). Created 0700; `FakeDisplay` removes it on drop.
    fn short_dir(name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::path::PathBuf::from("/tmp").join(format!("simee-{}-{n}-{name}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).expect("short dir");
        dir
    }

    pub(crate) fn fake_display(name: &str, size: (u32, u32), frames: u8, gap: Duration) -> FakeDisplay {
        let dir = short_dir(name);
        let state = emu_avd::state_dir(&dir);
        std::fs::create_dir_all(&state).expect("state dir");
        let listener = UnixListener::bind(state.join("display.sock")).expect("bind");
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        let (done, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else { return };
            // Wait for the test's go (or for the test to end).
            if go_rx.recv().is_err() {
                return;
            }
            // The client's request lines come first; any byte means it is connected.
            let mut hello = [0u8; 1];
            let _ = conn.read(&mut hello);
            for i in 0..frames {
                let header = FrameHeader::new(size.0, size.1, size.0, size.1, u64::from(i)).expect("header");
                let mut out = header.encode().to_vec();
                out.extend(vec![i; header.len as usize]);
                if conn.write_all(&out).is_err() {
                    return;
                }
                std::thread::sleep(gap);
            }
            // Hold the connection until the test ends.
            let _ = done_rx.recv();
        });
        FakeDisplay { dir, go, _done: done }
    }

    impl Drop for FakeDisplay {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Waits until the reader has the frame with sequence number `seq` in its mailbox.
    pub(crate) fn wait_for_seq(reader: &FrameReader, seq: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while reader.seqs().0 != Some(seq) {
            assert!(Instant::now() < deadline, "frame {seq} never arrived: {:?}", reader.seqs());
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn frames_are_reported_once_and_taken_newest_with_the_count_of_those_skipped() {
        // Five frames, sent back to back.
        let display = fake_display("reader", (4, 2), 5, Duration::ZERO);
        let mut reader = FrameReader::connect(&display.dir, 0).expect("connect");
        display.go();
        let first = reader.wait(Instant::now() + Duration::from_secs(10)).expect("wait").expect("a frame");
        let reported = reader.seqs().1.expect("wait reported one");
        // Let the rest arrive: the reader keeps only the newest.
        wait_for_seq(&reader, 4);
        let taken = reader.take().expect("a frame");
        assert_eq!((taken.width, taken.height, taken.rgba.len()), (4, 2, 32));
        assert!(taken.rgba.iter().all(|&b| b == 4), "the newest of five, which holds byte 4");
        assert!(taken.at >= first);
        assert_eq!(taken.unreported, 4 - reported, "the frames after the reported one that were replaced before `take`");
        assert_eq!(reader.device_size(), Some((4, 2)));
        assert!(reader.take().is_none(), "a frame is taken once");
        // Nothing new: wait times out rather than reporting the taken frame again.
        assert_eq!(reader.wait(Instant::now() + Duration::from_millis(30)).expect("wait"), None);
        // A buffer given back is the one the next frame is read into (the spare), not leaked.
        reader.recycle(taken.rgba);
    }

    #[test]
    fn a_closed_display_ends_the_wait_with_an_error() {
        let display = fake_display("closed", (2, 2), 1, Duration::ZERO);
        let mut reader = FrameReader::connect(&display.dir, 0).expect("connect");
        display.go();
        assert!(reader.wait(Instant::now() + Duration::from_secs(5)).expect("wait").is_some());
        reader.link.shutdown(std::net::Shutdown::Both).expect("shutdown");
        let err = reader.wait(Instant::now() + Duration::from_secs(5)).expect_err("closed");
        assert!(err.to_string().contains("display connection closed"), "{err}");
    }

    #[test]
    fn connecting_to_no_device_is_a_source_error() {
        let dir = std::env::temp_dir().join(format!("simee-encode-none-{}", std::process::id()));
        match FrameReader::connect(&dir, 0) {
            Err(Error::Source(m)) => assert!(m.contains("display.sock"), "{m}"),
            other => panic!("{:?}", other.err()),
        }
    }
}
