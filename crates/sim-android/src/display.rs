//! A client for a running emulator's display socket (`<avd>/simee/display.sock`, protocol in
//! `sim_core::stream`): input lines out, frames in. Frames arrive on the socket, or, for a client
//! that asked ([`Display::request_shared`]), in a shared ring file the emulator writes and this
//! side reads with `pread` ([`Frames`] takes either). A client that asked for IOSurfaces
//! (`Input::Surfaces`) gets their records from [`Frames::newest_into`]; the surface itself comes
//! over Mach, which this crate doesn't touch.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sim_core::hostfs::FileExt;
use sim_core::net::UnixStream;
use sim_core::rotation::Rotation;
use sim_core::stream::{FrameHeader, HEADER_LEN, Input, Record, ring};

/// The display socket of the AVD in `avd_dir`.
pub fn socket(avd_dir: &Path) -> PathBuf {
    emu_avd::state_dir(avd_dir).join("display.sock")
}

/// One connection to a display socket.
pub struct Display {
    stream: UnixStream,
}

impl Display {
    pub fn connect(avd_dir: &Path) -> Result<Display, String> {
        let path = socket(avd_dir);
        let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e} (is the device running with a display?)", path.display()))?;
        Ok(Display { stream })
    }

    /// Sends one input line.
    pub fn send(&mut self, input: &Input) -> Result<(), String> {
        let mut line = input.to_line();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    /// Asks for frames through the shared ring rather than the socket. Send it before `hello`;
    /// an emulator that predates the ring ignores it and keeps sending frames on the socket.
    pub fn request_shared(&mut self) -> Result<(), String> {
        self.send(&Input::Shared)
    }

    /// Asks for a turned device's frames as the panel lies, with how to turn them
    /// ([`Frames::rotation`]), for a client that turns them as it draws. Send it before `hello`;
    /// an emulator that predates it ignores it, and its frames are never turned.
    pub fn request_rotation(&mut self) -> Result<(), String> {
        self.send(&Input::Rotates)
    }

    /// A second handle on the same connection (for a reader thread).
    pub fn try_clone(&self) -> Result<UnixStream, String> {
        self.stream.try_clone().map_err(|e| e.to_string())
    }
}

/// Reads one frame: its header and RGBA rows. Only frames sent on the socket; [`Frames`] also
/// takes them from the shared ring.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<(FrameHeader, Vec<u8>)> {
    let mut hdr = [0u8; HEADER_LEN];
    r.read_exact(&mut hdr)?;
    let header = FrameHeader::decode(&hdr).map_err(std::io::Error::other)?;
    Ok((header, read_exactly(r, header.len as usize)?))
}

/// `len` bytes, read straight into reserved capacity: no zero-fill of a multi-megabyte frame
/// first.
fn read_exactly(r: &mut impl Read, len: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    read_exactly_into(r, len, &mut out)?;
    Ok(out)
}

/// [`read_exactly`] into `out`, replacing what it held: a buffer that already has room for `len`
/// bytes is refilled without allocating.
fn read_exactly_into(r: &mut impl Read, len: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
    out.clear();
    out.reserve_exact(len);
    r.take(len as u64).read_to_end(out)?;
    if out.len() != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// Bytes of socket input read ahead: room for 256 queued ring notices, so one `read` sees every
/// notice that piled up while the previous frame was being read and shown.
const READ_AHEAD: usize = 8 * 1024;

/// What [`Frames::newest_into`] read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// A frame whose RGBA rows are now in the buffer.
    Pixels(FrameHeader),
    /// A frame the server handed over as IOSurface `seq` (`sim_core::stream::Record::Surface`):
    /// its surface is in the client's Mach port, and the client sends `release <seq>` when done.
    Surface { header: FrameHeader, seq: u32 },
}

/// The frames of one display connection, wherever the server puts their pixels.
pub struct Frames<R> {
    /// Buffered, so the notices already waiting on the socket are visible without another
    /// syscall (a read returns what has arrived; it doesn't wait to fill the buffer).
    reader: BufReader<R>,
    ring: Option<Ring>,
    /// Where to ask for socket frames when an announced ring can't be opened.
    fallback: Option<UnixStream>,
    /// Frames announced in the ring but overwritten before they could be read (or in a ring
    /// that couldn't be opened).
    pub torn: u64,
    /// Ring frames skipped unread because a newer one was already announced behind them.
    pub superseded: u64,
    /// Frames passed over unread because nobody wanted them ([`Frames::next_frame_when`]).
    pub skipped: u64,
    /// How far to turn the frame just read, clockwise, to show it upright (after
    /// [`Display::request_rotation`]; always upright without it).
    pub rotation: Rotation,
}

impl<R: Read> Frames<R> {
    pub fn new(reader: R) -> Frames<R> {
        Frames {
            reader: BufReader::with_capacity(READ_AHEAD, reader),
            ring: None,
            fallback: None,
            torn: 0,
            superseded: 0,
            skipped: 0,
            rotation: Rotation::Deg0,
        }
    }

    /// The ring notice already read ahead behind the current record, if the next record is one.
    /// Only whole buffered records count: this never blocks or reads.
    fn queued_notice(&mut self) -> Option<(FrameHeader, u32)> {
        let raw: &[u8; HEADER_LEN] = self.reader.buffer().get(..HEADER_LEN)?.try_into().ok()?;
        match Record::decode(raw) {
            Ok(Record::Shared { header, slot }) => {
                self.reader.consume(HEADER_LEN);
                Some((header, slot))
            }
            _ => None,
        }
    }

    /// When an announced ring can't be opened (another user's file, a sandbox), ask for frames on
    /// the socket through `writer` (the connection's write side) instead of skipping them.
    pub fn with_fallback(mut self, writer: UnixStream) -> Frames<R> {
        self.fallback = Some(writer);
        self
    }

    /// The next whole frame, one per announcement, in order: its header and RGBA rows. Ring
    /// announcements are followed, and a frame the server overwrote before it was read (or that
    /// names a ring this side couldn't open) is skipped for the next one.
    pub fn next_frame(&mut self) -> std::io::Result<(FrameHeader, Vec<u8>)> {
        self.read_next(false, || true)
    }

    /// Like [`Frames::next_frame`], but a client that fell behind (a window showing frames) gets
    /// only the newest of the ring frames already announced: the ones before it are skipped
    /// unread (counted in `superseded`). Socket frames and ring announcements are never skipped.
    pub fn newest_frame(&mut self) -> std::io::Result<(FrameHeader, Vec<u8>)> {
        self.read_next(true, || true)
    }

    /// [`Frames::next_frame`], reading a frame's pixels only if `wanted()` says so when it
    /// arrives: one nobody wants (its window is hidden) is passed over without reading it from
    /// the ring (a socket frame's bytes are drained without keeping them).
    pub fn next_frame_when(&mut self, wanted: impl Fn() -> bool) -> std::io::Result<(FrameHeader, Vec<u8>)> {
        self.read_next(false, wanted)
    }

    /// [`Frames::newest_frame`] with [`Frames::next_frame_when`]'s `wanted()`: a shown window gets
    /// the newest announced frame, a hidden one reads none.
    pub fn newest_frame_when(&mut self, wanted: impl Fn() -> bool) -> std::io::Result<(FrameHeader, Vec<u8>)> {
        self.read_next(true, wanted)
    }

    /// [`Frames::newest_frame_when`], reading the frame's pixels into `buf` (replacing what it
    /// held) rather than a new buffer: a window that gets its previous frame's buffer back
    /// (Rune's `Pixels::replace`) refills it, so a steady stream allocates nothing per frame.
    /// `buf` holds nothing useful after an error.
    pub fn newest_frame_into(&mut self, buf: &mut Vec<u8>, wanted: impl Fn() -> bool) -> std::io::Result<FrameHeader> {
        self.read_next_into(true, wanted, buf)
    }

    /// [`Frames::newest_frame_into`] for a client that asked for IOSurfaces: a surface record is
    /// returned as it is ([`Next::Surface`], never skipped, since each one has to be released),
    /// and any other frame's pixels are read into `buf`.
    pub fn newest_into(&mut self, buf: &mut Vec<u8>, wanted: impl Fn() -> bool) -> std::io::Result<Next> {
        self.read_record(true, wanted, buf, true)
    }

    fn read_next(&mut self, skip_to_newest: bool, wanted: impl Fn() -> bool) -> std::io::Result<(FrameHeader, Vec<u8>)> {
        let mut rgba = Vec::new();
        let header = self.read_next_into(skip_to_newest, wanted, &mut rgba)?;
        Ok((header, rgba))
    }

    fn read_next_into(&mut self, skip_to_newest: bool, wanted: impl Fn() -> bool, buf: &mut Vec<u8>) -> std::io::Result<FrameHeader> {
        match self.read_record(skip_to_newest, wanted, buf, false)? {
            Next::Pixels(header) => Ok(header),
            // Never returned without `surfaces`.
            Next::Surface { .. } => Err(std::io::Error::other("unexpected surface frame")),
        }
    }

    /// The next frame; surface records are returned when `surfaces`, else passed over (a client
    /// that never asked for them gets none).
    fn read_record(&mut self, skip_to_newest: bool, wanted: impl Fn() -> bool, buf: &mut Vec<u8>, surfaces: bool) -> std::io::Result<Next> {
        loop {
            let mut hdr = [0u8; HEADER_LEN];
            self.reader.read_exact(&mut hdr)?;
            match Record::decode(&hdr).map_err(std::io::Error::other)? {
                Record::Frame(header) if !wanted() => {
                    let len = u64::from(header.len);
                    if std::io::copy(&mut (&mut self.reader).take(len), &mut std::io::sink())? != len {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                    self.skipped += 1;
                }
                Record::Frame(header) => {
                    read_exactly_into(&mut self.reader, header.len as usize, buf)?;
                    return Ok(Next::Pixels(header));
                }
                Record::Surface { header, seq } if surfaces => return Ok(Next::Surface { header, seq }),
                Record::Surface { .. } => self.skipped += 1,
                Record::Ring { path_len } => {
                    let path = read_exactly(&mut self.reader, path_len as usize)?;
                    // A ring that can't be opened leaves none: its frames are skipped, and the
                    // server is asked to send frames on the socket if there's a way to ask.
                    self.ring = String::from_utf8(path).ok().and_then(|p| Ring::open(Path::new(&p)).ok());
                    if self.ring.is_none()
                        && let Some(w) = self.fallback.as_mut()
                    {
                        w.write_all(format!("{}\n", Input::Socket.to_line()).as_bytes())?;
                    }
                }
                // Before the frames it applies to; a frame ahead of it keeps the turn it had.
                Record::Rotation(r) => self.rotation = r,
                Record::Shared { .. } if !wanted() => self.skipped += 1,
                Record::Shared { mut header, mut slot } => {
                    // A client that fell behind reads only the newest of the frames announced
                    // so far: the older ones would be replaced before anything showed them.
                    while skip_to_newest && let Some(newer) = self.queued_notice() {
                        self.superseded += 1;
                        (header, slot) = newer;
                    }
                    match self.ring.as_ref().map(|r| r.read_into(slot, &header, buf)) {
                        Some(Ok(true)) => return Ok(Next::Pixels(header)),
                        Some(Err(e)) => return Err(e),
                        Some(Ok(false)) | None => self.torn += 1,
                    }
                }
            }
        }
    }
}

/// A shared frame ring, open for reading.
struct Ring {
    file: File,
    layout: ring::Layout,
}

impl Ring {
    fn open(path: &Path) -> std::io::Result<Ring> {
        let file = File::open(path)?;
        let mut head = [0u8; ring::FILE_HEADER_LEN];
        file.read_exact_at(&mut head, 0)?;
        let layout = ring::Layout::decode(&head).map_err(std::io::Error::other)?;
        Ok(Ring { file, layout })
    }

    /// Reads slot `slot`'s pixels into `buf`: `true` if it still held the frame `header`
    /// announced, whole; `false` if it was being (or has been) rewritten.
    fn read_into(&self, slot: u32, header: &FrameHeader, buf: &mut Vec<u8>) -> std::io::Result<bool> {
        if slot >= self.layout.slots || u64::from(header.len) > self.layout.capacity {
            return Err(std::io::Error::other(format!("frame ring slot {slot} out of range")));
        }
        let at = self.layout.slot_header_offset(slot);
        let mut raw = [0u8; ring::SLOT_HEADER_LEN];
        self.file.read_exact_at(&mut raw, at)?;
        let before = ring::Slot::decode(&raw).unwrap_or_default();
        if before.len != header.len || !before.read_is_whole(before.seq, header.frame) {
            return Ok(false);
        }
        // The seqlock reader's barriers: the pixels are read after the header, and the second
        // read of the sequence number after the pixels (the syscalls order them in practice).
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let mut f = &self.file;
        f.seek(SeekFrom::Start(self.layout.slot_data_offset(slot)))?;
        read_exactly_into(&mut f, header.len as usize, buf)?;
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let mut seq = [0u8; 8];
        self.file.read_exact_at(&mut seq, at)?;
        Ok(before.read_is_whole(u64::from_le_bytes(seq), header.frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use sim_core::gesture::Phase;
    #[cfg(unix)]
    use sim_core::net::UnixListener;
    #[cfg(unix)]
    use std::io::{BufRead, BufReader};

    // Sockets under /tmp: Windows has no Unix sockets here yet.
    #[cfg(unix)]
    #[test]
    fn sends_input_lines_and_reads_frames() {
        // Under /tmp: the socket path must fit a unix socket address (104 bytes on macOS).
        let dir = PathBuf::from("/tmp").join(format!("sa-display-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(socket(&dir)).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let got: Vec<String> = (0..3).map(|_| lines.next().expect("line").expect("utf8")).collect();
            let h = FrameHeader { width: 1, height: 1, device_width: 2, device_height: 2, frame: 1, len: 4 };
            conn.write_all(&h.encode()).and_then(|()| conn.write_all(&[1, 2, 3, 4])).expect("frame");
            got
        });
        let mut d = Display::connect(&dir).expect("connect");
        d.request_shared().expect("shm");
        d.send(&Input::Hello { max_width: 720 }).expect("hello");
        d.send(&Input::Touch { x: 5.0, y: 6.0, phase: Phase::Down }).expect("touch");
        let (h, rgba) = read_frame(&mut d.try_clone().expect("clone")).expect("frame");
        // A frame cut short is an error, not a short frame.
        let cut = FrameHeader { width: 1, height: 2, device_width: 2, device_height: 2, frame: 2, len: 8 };
        let mut short = std::io::Cursor::new([cut.encode().to_vec(), vec![1; 5]].concat());
        assert_eq!(read_frame(&mut short).map(|_| ()).map_err(|e| e.kind()), Err(std::io::ErrorKind::UnexpectedEof));
        assert_eq!((h.device_width, rgba), (2, vec![1, 2, 3, 4]));
        assert_eq!(server.join().expect("server"), vec!["shm".to_owned(), "hello 720".to_owned(), "touch 5 6 down".to_owned()]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_missing_socket_says_so() {
        let err = Display::connect(Path::new("/nonexistent-avd")).err().expect("no socket");
        assert!(err.contains("display.sock") && err.contains("is the device running"), "{err}");
    }

    /// Writes a ring file the way the server does, with plain file writes.
    fn write_ring(path: &Path, layout: ring::Layout, slots: &[(u32, ring::Slot, &[u8])]) {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).expect("ring");
        file.set_len(layout.file_len()).expect("len");
        file.write_all_at(&layout.encode(), 0).expect("header");
        for (i, slot, px) in slots {
            file.write_all_at(&slot.encode(), layout.slot_header_offset(*i)).expect("slot");
            file.write_all_at(px, layout.slot_data_offset(*i)).expect("pixels");
        }
    }

    /// A client that fell behind: the server announces 8 phone-sized frames in one burst before
    /// the client reads any. Prints the time to catch up per burst and the frames read.
    /// `cargo test --release -p simee-android display_burst_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench"]
    fn display_burst_bench() {
        const SLOTS: u32 = 8;
        const BURSTS: usize = 40;
        let (w, h) = (1080u32, 2400u32);
        let len = w * h * 4;
        let dir = PathBuf::from("/tmp").join(format!("sa-burst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("frames.ring");
        let layout = ring::Layout::new(SLOTS, u64::from(len));
        let px = vec![7u8; len as usize];
        let slots: Vec<(u32, ring::Slot, &[u8])> = (0..SLOTS)
            .map(|i| {
                let slot = ring::Slot { seq: 2, frame: u64::from(i), width: w, height: h, device_width: w, device_height: h, len };
                (i, slot, px.as_slice())
            })
            .collect();
        write_ring(&path, layout, &slots);
        // Every frame in order (`next_frame`), then the window's path: a new buffer per frame
        // (`newest_frame`), and the buffer the frame before last came back in
        // (`newest_frame_into`, two alternating as `Pixels::replace` hands them back).
        for how in [How::Next, How::Newest, How::NewestInto] {
            burst_run(&path, how, SLOTS, BURSTS, (w, h));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[derive(Clone, Copy, Debug)]
    enum How {
        Next,
        Newest,
        NewestInto,
    }

    fn burst_run(path: &Path, how: How, slots: u32, bursts: usize, (w, h): (u32, u32)) {
        let len = w * h * 4;
        let (client, mut server) = UnixStream::pair().expect("pair");
        let (caught_up, wait) = std::sync::mpsc::channel::<()>();
        let p = path.to_str().expect("utf8").as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            let mut ring = Record::Ring { path_len: p.len() as u32 }.encode().to_vec();
            ring.extend(&p);
            server.write_all(&ring).expect("ring");
            let mut burst = Vec::new();
            for i in 0..slots {
                let header = FrameHeader::new(w, h, w, h, u64::from(i)).expect("size");
                burst.extend(Record::Shared { header, slot: i }.encode());
            }
            for _ in 0..bursts {
                server.write_all(&burst).expect("burst");
                wait.recv().expect("caught up");
            }
        });
        let mut frames = Frames::new(client);
        let (mut read, mut times) = (0usize, Vec::new());
        let mut bufs = [Vec::new(), Vec::new()];
        for _ in 0..bursts {
            let start = std::time::Instant::now();
            loop {
                let header = match how {
                    How::Next => frames.next_frame().map(|(h, px)| (h, px.len())),
                    // Held until the next frame replaces it, as the window's `Pixels` holds it.
                    How::Newest => frames.newest_frame().map(|(h, px)| {
                        let n = px.len();
                        bufs[0] = px;
                        (h, n)
                    }),
                    How::NewestInto => frames.newest_frame_into(&mut bufs[read % 2], || true).map(|h| (h, bufs[read % 2].len())),
                };
                let (header, got) = header.expect("frame");
                assert_eq!(got, len as usize);
                read += 1;
                if header.frame == u64::from(slots - 1) {
                    break;
                }
            }
            times.push(start.elapsed());
            caught_up.send(()).expect("send");
        }
        writer.join().expect("writer");
        times.sort();
        println!(
            "{:?}: burst of {slots} {w}x{h} notices: median {:?} to catch up, {} frames read ({:.1} per burst, {:.0} MB per burst)",
            how,
            times[bursts / 2],
            read,
            read as f64 / bursts as f64,
            read as f64 / bursts as f64 * f64::from(len) / 1e6
        );
    }

    /// A reader that hands out at most one record header's worth of bytes per read.
    #[cfg(unix)]
    struct Trickle<R>(R);

    #[cfg(unix)]
    impl<R: Read> Read for Trickle<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(HEADER_LEN);
            self.0.read(&mut buf[..n])
        }
    }

    /// A fake emulator announces 120 frames at once (the window was busy): the client reads
    /// only the newest from the ring, once.
    #[cfg(unix)]
    #[test]
    fn a_backlog_of_notices_reads_only_the_newest_frame() {
        let dir = PathBuf::from("/tmp").join(format!("sa-backlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("frames.ring");
        let layout = ring::Layout::new(3, 8);
        let slot = |frame| ring::Slot { seq: 2, frame, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
        // The ring holds the last three frames announced, frame n in slot n % 3.
        let (old, newest) = ([1u8; 8], [2u8; 8]);
        write_ring(&path, layout, &[(1, slot(118), &old), (2, slot(119), &old), (0, slot(120), &newest)]);
        let header = |frame| FrameHeader::new(2, 1, 4, 2, frame).expect("size");
        let p = path.to_str().expect("utf8").as_bytes();
        let mut msg = Record::Ring { path_len: p.len() as u32 }.encode().to_vec();
        msg.extend(p);
        for n in 1..=120u64 {
            msg.extend(Record::Shared { header: header(n), slot: (n % 3) as u32 }.encode());
        }
        let (client, mut server) = UnixStream::pair().expect("pair");
        server.write_all(&msg).expect("send");
        drop(server);
        let mut frames = Frames::new(client);
        assert_eq!(frames.newest_frame().expect("frame"), (header(120), newest.to_vec()));
        assert_eq!((frames.superseded, frames.torn), (119, 0), "one ring read for 120 notices");
        assert!(frames.newest_frame().is_err(), "nothing else announced");
        // `next_frame` keeps its meaning: one frame per notice, in order (frames 1-117 were
        // overwritten in the 3-slot ring, so they're torn, not skipped).
        let mut frames = Frames::new(std::io::Cursor::new(msg.clone()));
        let got: Vec<u64> = std::iter::from_fn(|| frames.next_frame().ok()).map(|(h, _)| h.frame).collect();
        assert_eq!(got, vec![118, 119, 120]);
        assert_eq!((frames.superseded, frames.torn), (0, 117));
        // Neighbours: a socket frame or a new ring behind the notices stops the skipping, and
        // the newest notice still wins over the ones before it.
        let mut msg = Record::Ring { path_len: p.len() as u32 }.encode().to_vec();
        msg.extend(p);
        msg.extend(Record::Shared { header: header(119), slot: 2 }.encode());
        msg.extend(Record::Frame(header(121)).encode());
        msg.extend([9; 8]);
        msg.extend(Record::Shared { header: header(118), slot: 1 }.encode());
        msg.extend(Record::Shared { header: header(120), slot: 0 }.encode());
        let mut frames = Frames::new(std::io::Cursor::new(msg));
        assert_eq!(frames.newest_frame().expect("ring frame").0, header(119));
        assert_eq!(frames.newest_frame().expect("socket frame"), (header(121), vec![9; 8]));
        assert_eq!(frames.newest_frame().expect("newest"), (header(120), newest.to_vec()));
        assert_eq!((frames.superseded, frames.torn), (1, 0));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn surface_records_go_to_clients_that_take_them_and_past_the_rest() {
        let header = |frame| FrameHeader::new(2, 1, 2, 1, frame).expect("size");
        let mut msg = Record::Surface { header: header(1), seq: 10 }.encode().to_vec();
        msg.extend(Record::Surface { header: header(2), seq: 11 }.encode());
        msg.extend(Record::Frame(header(3)).encode());
        msg.extend([7; 8]);
        // Every surface record is returned, the newest not skipping the older: each one has to be
        // released. A socket frame after them still comes with its pixels.
        let mut frames = Frames::new(std::io::Cursor::new(msg.clone()));
        let mut buf = Vec::new();
        assert_eq!(frames.newest_into(&mut buf, || true).expect("first"), Next::Surface { header: header(1), seq: 10 });
        assert_eq!(frames.newest_into(&mut buf, || true).expect("second"), Next::Surface { header: header(2), seq: 11 });
        assert_eq!(frames.newest_into(&mut buf, || true).expect("pixels"), Next::Pixels(header(3)));
        assert_eq!(buf, vec![7; 8]);
        // A client reading only pixels passes them over.
        let mut frames = Frames::new(std::io::Cursor::new(msg));
        assert_eq!(frames.newest_frame().expect("pixels"), (header(3), vec![7; 8]));
        assert_eq!(frames.skipped, 2);
    }

    #[cfg(unix)]
    #[test]
    fn frames_come_from_the_socket_or_the_ring() {
        let dir = PathBuf::from("/tmp").join(format!("sa-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("frames.ring");
        let layout = ring::Layout::new(3, 8);
        let px = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let slot = |frame, seq| ring::Slot { seq, frame, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
        write_ring(&path, layout, &[(1, slot(5, 2), &px), (2, slot(6, 3), &px), (0, slot(4, 2), &px)]);
        let header = |frame| FrameHeader::new(2, 1, 4, 2, frame).expect("size");
        let p = path.to_str().expect("utf8").as_bytes();
        let mut stream = Vec::new();
        // A shared frame before any ring is announced: skipped.
        stream.extend(Record::Shared { header: header(3), slot: 0 }.encode());
        stream.extend(Record::Ring { path_len: p.len() as u32 }.encode());
        stream.extend(p);
        stream.extend(Record::Shared { header: header(5), slot: 1 }.encode());
        // Slot 2 is mid-write (odd sequence) and slot 0 holds another frame: both skipped.
        stream.extend(Record::Shared { header: header(6), slot: 2 }.encode());
        stream.extend(Record::Shared { header: header(9), slot: 0 }.encode());
        stream.extend(Record::Frame(header(10)).encode());
        stream.extend([9; 8]);
        stream.extend(Record::Shared { header: header(11), slot: 7 }.encode());
        // One record per read: a client keeping up, so no notice is ever queued behind another.
        let mut frames = Frames::new(Trickle(std::io::Cursor::new(stream)));
        assert_eq!(frames.next_frame().expect("ring frame"), (header(5), px.to_vec()));
        assert_eq!(frames.next_frame().expect("socket frame"), (header(10), vec![9; 8]));
        assert_eq!(frames.torn, 3);
        let err = frames.next_frame().expect_err("slot out of range");
        assert!(err.to_string().contains("slot 7 out of range"), "{err}");
        assert!(frames.next_frame().is_err(), "end of stream");
        // A ring that can't be opened (gone, or not a ring) leaves its frames skipped.
        let mut stream = Vec::new();
        let gone = dir.join("gone.ring");
        let g = gone.to_str().expect("utf8").as_bytes();
        stream.extend(Record::Ring { path_len: g.len() as u32 }.encode());
        stream.extend(g);
        stream.extend(Record::Shared { header: header(1), slot: 0 }.encode());
        let (asks, server) = UnixStream::pair().expect("pair");
        let mut frames = Frames::new(std::io::Cursor::new(stream)).with_fallback(asks);
        assert!(frames.next_frame().is_err());
        assert_eq!(frames.torn, 1);
        let mut line = String::new();
        BufReader::new(server).read_line(&mut line).expect("line");
        assert_eq!(line, "socket\n", "asked for socket frames");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A window hands back the buffer its previous frame was in: ring and socket frames are read
    /// into that same allocation, so a steady stream allocates nothing per frame (#17).
    #[test]
    fn frames_refill_a_recycled_buffer_in_place() {
        let dir = PathBuf::from("/tmp").join(format!("sa-recycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("frames.ring");
        let layout = ring::Layout::new(2, 8);
        let slot = |frame| ring::Slot { seq: 2, frame, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
        write_ring(&path, layout, &[(0, slot(1), &[1; 8]), (1, slot(3), &[3; 8])]);
        let header = |frame| FrameHeader::new(2, 1, 4, 2, frame).expect("size");
        let p = path.to_str().expect("utf8").as_bytes();
        let mut stream = Vec::new();
        stream.extend(Record::Ring { path_len: p.len() as u32 }.encode());
        stream.extend(p);
        stream.extend(Record::Shared { header: header(1), slot: 0 }.encode());
        stream.extend(Record::Frame(header(2)).encode());
        stream.extend([2; 8]);
        stream.extend(Record::Shared { header: header(3), slot: 1 }.encode());
        let mut frames = Frames::new(std::io::Cursor::new(stream));
        let mut buf = vec![0xee; 8];
        let at = buf.as_ptr();
        for n in 1..=3u8 {
            let h = frames.newest_frame_into(&mut buf, || true).expect("frame");
            assert_eq!((h, buf.as_slice()), (header(u64::from(n)), &[n; 8][..]));
            assert_eq!(buf.as_ptr(), at, "frame {n} was read into the same buffer");
        }
        assert!(frames.newest_frame_into(&mut buf, || true).is_err(), "end of stream");
        // An empty buffer (the first frame) is filled without zeroing it first.
        let mut frames = Frames::new(std::io::Cursor::new([Record::Frame(header(4)).encode().to_vec(), vec![4; 8]].concat()));
        let mut empty = Vec::new();
        assert_eq!(frames.newest_frame_into(&mut empty, || true).expect("frame"), header(4));
        assert_eq!(empty, [4; 8]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unwanted_frames_are_passed_over_unread() {
        let dir = PathBuf::from("/tmp").join(format!("sa-unwanted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("frames.ring");
        let layout = ring::Layout::new(2, 8);
        let slot = |frame| ring::Slot { seq: 2, frame, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
        write_ring(&path, layout, &[(0, slot(1), &[1; 8]), (1, slot(2), &[2; 8])]);
        let header = |frame| FrameHeader::new(2, 1, 4, 2, frame).expect("size");
        let p = path.to_str().expect("utf8").as_bytes();
        let mut stream = Vec::new();
        stream.extend(Record::Ring { path_len: p.len() as u32 }.encode());
        stream.extend(p);
        stream.extend(Record::Shared { header: header(1), slot: 0 }.encode());
        stream.extend(Record::Frame(header(3)).encode());
        stream.extend([3; 8]);
        stream.extend(Record::Shared { header: header(2), slot: 1 }.encode());
        // Wanted again only from the third frame on.
        let asked = std::cell::Cell::new(0);
        let wanted = || {
            asked.set(asked.get() + 1);
            asked.get() > 2
        };
        let mut frames = Frames::new(std::io::Cursor::new(stream));
        assert_eq!(frames.next_frame_when(wanted).expect("frame"), (header(2), vec![2; 8]), "the socket frame's bytes were drained");
        assert_eq!((frames.skipped, frames.torn), (2, 0));
        std::fs::remove_dir_all(dir).ok();
    }
}
