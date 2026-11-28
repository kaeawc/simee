//! The frame source on Linux: emu's display socket and frame ring, the same stream the macOS
//! [`crate::AndroidSource`] reads, as pictures for a software encoder. emu's frames are RGBA rows;
//! the picture is that frame itself (no copy: the buffer the reader filled is the buffer the encoder
//! reads, and the previous picture's buffer goes back to the reader).

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use sim_core::pixels::{downscale_factor, downscaled_size, hello_width_for};
use sim_core::stream::Input;

use crate::android_frames::FrameReader;
use crate::i420::{Picture, PixelOrder};
use crate::stream::Source;
use crate::{Error, Result};

/// A running AVD's display (`<avd>/simee/display.sock`), frames through the shared ring, paced by
/// emu's frame announcements.
pub struct AndroidSource {
    reader: FrameReader,
    current: Option<Picture>,
    /// What the last `picture` call that took a frame saw, until the stream loop reads it.
    taken_at: Option<Instant>,
    unreported: u64,
    /// The stream's longest side and the `hello` width last sent for it, when frames are asked
    /// for at that size ([`AndroidSource::connect_fitted`]).
    fit: Option<Fit>,
    /// After a new request on a running stream: the size its frames will have, and how many more
    /// frames to hold back while emu still sends the old shape's.
    hold: Option<Hold>,
}

#[derive(Clone, Copy)]
struct Hold {
    expected: (u32, u32),
    left: u32,
}

/// Frames held back after a new request at most, for an emu that ignores it.
const HOLD_FRAMES: u32 = 15;

#[derive(Clone, Copy)]
struct Fit {
    max_size: u32,
    hello: u32,
}

impl AndroidSource {
    /// Connects to the AVD in `avd_dir` and asks for frames at most `max_width` pixels wide (0:
    /// full size; emu shrinks by a whole factor, `ceil(width / max_width)`).
    pub fn connect(avd_dir: &Path, max_width: u32) -> Result<AndroidSource> {
        Ok(AndroidSource {
            reader: FrameReader::connect(avd_dir, max_width)?,
            current: None,
            taken_at: None,
            unreported: 0,
            fit: None,
            hold: None,
        })
    }

    /// Connects and asks emu for frames whose longest side is at or over `max_size`, as close to
    /// it as a whole shrink factor allows, so the encoder has little left to scale (and never has
    /// to scale up). `screen` is the device's display size, when known:
    /// with it the first frame already comes at that size, without it the first frame is full
    /// size. The request follows the display (a rotation turns the width): it is sent again when
    /// the frames show the shape changed. An emu that ignores the request sends full frames, which
    /// the encoder scales as it always did.
    pub fn connect_fitted(avd_dir: &Path, max_size: u32, screen: Option<(u32, u32)>) -> Result<AndroidSource> {
        let hello = screen.map_or(0, |size| hello_width_for(size, max_size));
        let mut source = Self::connect(avd_dir, hello)?;
        source.fit = Some(Fit { max_size, hello });
        Ok(source)
    }

    /// The device's own display size (what input coordinates are in), once a frame has arrived.
    pub fn device_size(&self) -> Option<(u32, u32)> {
        self.reader.device_size()
    }

    /// A second write handle on the connection, for sending input lines from another thread.
    pub fn input_link(&self) -> Result<UnixStream> {
        self.reader.input_link()
    }

    /// Sends one input line (a touch, a key) on the display connection.
    pub fn input(&mut self, input: &Input) -> Result<()> {
        self.reader.input(input)
    }
}

impl AndroidSource {
    /// Sends a new `hello` width when the display's shape (known from the device's size and the
    /// frame's orientation) needs one, and returns the size the frames will have once emu
    /// applies it.
    fn refit(&mut self, portrait: bool) -> Option<(u32, u32)> {
        let (Some(fit), Some((w, h))) = (self.fit, self.reader.device_size()) else { return None };
        let (short, long) = (w.min(h), w.max(h));
        let upright = if portrait { (short, long) } else { (long, short) };
        let hello = hello_width_for(upright, fit.max_size);
        if hello == fit.hello || self.reader.input(&Input::Hello { max_width: hello }).is_err() {
            return None;
        }
        self.fit = Some(Fit { hello, ..fit });
        Some(downscaled_size(upright.0, upright.1, downscale_factor(upright.0, hello)))
    }
}

impl Source for AndroidSource {
    type Picture = Picture;

    fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
        self.reader.wait(deadline)
    }

    fn picture(&mut self) -> Result<Option<&Picture>> {
        if let Some(frame) = self.reader.take() {
            let (width, height) = (frame.width, frame.height);
            if frame.rgba.len() != width as usize * height as usize * 4 {
                return Err(Error::Source(format!("a {width}x{height} frame of {} bytes", frame.rgba.len())));
            }
            if let Some(hold) = self.hold.as_mut() {
                if (width, height) != hold.expected && hold.left > 0 {
                    // Still the old shape's size: the encoder restarts once, at the new one.
                    hold.left -= 1;
                    self.unreported += frame.unreported + 1;
                    self.reader.recycle(frame.rgba);
                    return Ok(self.current.as_ref());
                }
                self.hold = None;
            } else if let Some(expected) = self.refit(width <= height) {
                // This frame is the old shape's too. With nothing shown yet there is nothing to
                // restart, so it goes out.
                if self.current.is_some() {
                    self.hold = Some(Hold { expected, left: HOLD_FRAMES });
                    self.unreported += frame.unreported + 1;
                    self.reader.recycle(frame.rgba);
                    return Ok(self.current.as_ref());
                }
            }
            self.unreported += frame.unreported;
            self.taken_at = Some(frame.at);
            let picture = Picture::new(width, height, PixelOrder::Rgba, frame.rgba);
            if let Some(old) = self.current.replace(picture) {
                self.reader.recycle(old.data);
            }
        }
        Ok(self.current.as_ref())
    }

    fn take_captured(&mut self) -> Option<Instant> {
        self.taken_at.take()
    }

    fn take_unreported(&mut self) -> u64 {
        std::mem::take(&mut self.unreported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::android_frames::tests::{fake_display, short_dir, wait_for_seq};
    use crate::config::output_size;
    use sim_core::pixels::{downscale_factor, downscaled_size};
    use sim_core::stream::FrameHeader;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// An emu display that records the client's lines and sends one frame per `step`: of the
    /// device's upright display (`.1`, on a device whose own size is `.0`), shrunk by the whole
    /// factor the last `hello` asks for, or at full size when it `honours` nothing.
    struct Emu {
        dir: std::path::PathBuf,
        lines: Arc<Mutex<Vec<String>>>,
        step: Sender<()>,
    }

    impl Emu {
        fn new(honours: bool, script: Vec<((u32, u32), (u32, u32))>) -> Emu {
            let dir = short_dir("fitted");
            std::fs::create_dir_all(emu_avd::state_dir(&dir)).expect("state dir");
            let listener = UnixListener::bind(emu_avd::state_dir(&dir).join("display.sock")).expect("bind");
            let lines = Arc::new(Mutex::new(Vec::<String>::new()));
            let (step, steps) = channel::<()>();
            let seen = lines.clone();
            std::thread::spawn(move || {
                let Ok((mut conn, _)) = listener.accept() else { return };
                let reader = conn.try_clone().expect("clone");
                let recorded = seen.clone();
                std::thread::spawn(move || {
                    for line in BufReader::new(reader).lines().map_while(Result::ok) {
                        recorded.lock().expect("lines").push(line);
                    }
                });
                // The last `hello` line and how many there are.
                let latest = || {
                    let lines = seen.lock().expect("lines");
                    let hellos: Vec<u32> = lines.iter().filter_map(|l| l.strip_prefix("hello ")?.parse().ok()).collect();
                    (hellos.last().copied(), hellos.len())
                };
                let mut send = |seq: u64, device: (u32, u32), upright: (u32, u32), hello: u32| {
                    let factor = if honours { downscale_factor(upright.0, hello) } else { 1 };
                    let (w, h) = downscaled_size(upright.0, upright.1, factor);
                    let header = FrameHeader::new(w, h, device.0, device.1, seq).expect("header");
                    let mut out = header.encode().to_vec();
                    out.extend(vec![seq as u8; header.len as usize]);
                    conn.write_all(&out).is_ok()
                };
                let mut last = None;
                let mut sent_at = 0;
                let mut seq = 0;
                for (device, upright) in script {
                    if steps.recv().is_err() {
                        return;
                    }
                    let hello = loop {
                        if let (Some(h), n) = latest() {
                            sent_at = n;
                            break h;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    };
                    if !send(seq, device, upright, hello) {
                        return;
                    }
                    (last, seq) = (Some((device, upright)), seq + 1);
                }
                // The screen draws nothing more. Like emu, send the newest frame again once the
                // client asks for another size, and not before.
                while let (Some((device, upright)), Ok(())) = (last, steps.recv()) {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while latest().1 <= sent_at {
                        if Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let hello = latest().0.expect("a hello");
                    sent_at = latest().1;
                    if !send(seq, device, upright, hello) {
                        return;
                    }
                    seq += 1;
                }
                // Hold the connection until the test ends (the step sender drops).
                let _ = steps.recv();
            });
            Emu { dir, lines, step }
        }

        fn hellos(&self) -> Vec<String> {
            self.lines.lock().expect("lines").iter().filter(|l| l.starts_with("hello ")).cloned().collect()
        }

        /// The `hello` lines once there are `n` of them.
        fn hellos_at_least(&self, n: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.hellos().len() < n {
                assert!(Instant::now() < deadline, "{n} hellos never arrived: {:?}", self.hellos());
                std::thread::sleep(Duration::from_millis(2));
            }
            self.hellos()
        }

        /// Lets the next frame go and returns the picture's size once it arrived.
        fn next_picture(&self, source: &mut AndroidSource) -> (u32, u32) {
            self.step.send(()).expect("emu is running");
            source.wait(Instant::now() + Duration::from_secs(10)).expect("wait").expect("a frame");
            source.picture().expect("picture").expect("a picture").size()
        }
    }

    impl Drop for Emu {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const PHONE: (u32, u32) = (64, 128);

    #[test]
    fn a_fitted_source_asks_emu_for_the_width_that_keeps_the_longest_side_at_or_over_max_size() {
        let emu = Emu::new(true, vec![(PHONE, PHONE)]);
        let _source = AndroidSource::connect_fitted(&emu.dir, 64, Some(PHONE)).expect("connect");
        assert_eq!(emu.hellos_at_least(1), ["hello 32"], "factor 2 for a 128-pixel side under 64");
    }

    #[test]
    fn an_emu_that_honours_the_hint_sends_frames_the_encoder_has_no_scaling_to_do_for() {
        let emu = Emu::new(true, vec![(PHONE, PHONE), (PHONE, PHONE)]);
        let mut source = AndroidSource::connect_fitted(&emu.dir, 64, Some(PHONE)).expect("connect");
        let size = emu.next_picture(&mut source);
        assert_eq!(size, (32, 64));
        assert_eq!(output_size(size, Some(64)), size, "no CPU downscale left");
        assert_eq!(emu.next_picture(&mut source), (32, 64));
        assert_eq!(emu.hellos(), ["hello 32"], "the display's shape didn't change: the request isn't repeated");
    }

    #[test]
    fn an_emu_that_ignores_the_hint_still_yields_frames_the_encoder_scales_to_the_right_size() {
        let emu = Emu::new(false, vec![(PHONE, PHONE), (PHONE, PHONE)]);
        let mut source = AndroidSource::connect_fitted(&emu.dir, 64, Some(PHONE)).expect("connect");
        let size = emu.next_picture(&mut source);
        assert_eq!(size, PHONE, "full-size frames");
        assert_eq!(output_size(size, Some(64)), (32, 64), "the encoder's scaling gives the stream's size");
        assert_eq!(emu.next_picture(&mut source), PHONE);
        assert_eq!(emu.hellos(), ["hello 32"], "an ignored request isn't repeated on every frame");
    }

    #[test]
    fn without_the_screen_size_the_first_frame_is_full_size_and_the_next_request_follows_it() {
        let emu = Emu::new(true, vec![(PHONE, PHONE), (PHONE, PHONE)]);
        let mut source = AndroidSource::connect_fitted(&emu.dir, 64, None).expect("connect");
        assert_eq!(emu.next_picture(&mut source), PHONE, "asked for full size");
        assert_eq!(emu.hellos_at_least(2), ["hello 4294967295", "hello 32"]);
        assert_eq!(emu.next_picture(&mut source), (32, 64));
    }

    #[test]
    fn a_rotation_changes_the_request_and_the_encoder_sees_the_new_size_once() {
        let landscape = (128, 64);
        let emu = Emu::new(true, vec![(PHONE, PHONE), (landscape, landscape), (landscape, landscape)]);
        let mut source = AndroidSource::connect_fitted(&emu.dir, 64, Some(PHONE)).expect("connect");
        assert_eq!(emu.next_picture(&mut source), (32, 64));
        // The device turned: emu's next frame is upright landscape, shrunk by the portrait
        // request (128 / 4 = 32 wide), which is nobody's size. It is held back, the picture stays
        // the last one, and the request for the landscape width goes out.
        assert_eq!(emu.next_picture(&mut source), (32, 64), "the in-between size never reaches the encoder");
        assert_eq!(emu.hellos_at_least(2), ["hello 32", "hello 64"]);
        assert_eq!(emu.next_picture(&mut source), (64, 32), "then the new shape's own size");
    }

    #[test]
    fn a_rotation_on_a_static_screen_still_shows_the_turned_picture() {
        let landscape = (128, 64);
        // One frame in each shape and no more: the screen draws nothing after the turn, so the
        // new picture comes only from emu sending the turned frame again at the new size.
        let emu = Emu::new(true, vec![(PHONE, PHONE), (landscape, landscape)]);
        let mut source = AndroidSource::connect_fitted(&emu.dir, 64, Some(PHONE)).expect("connect");
        assert_eq!(emu.next_picture(&mut source), (32, 64));
        assert_eq!(emu.next_picture(&mut source), (32, 64), "the turned frame at the old size is held");
        assert_eq!(emu.hellos_at_least(2), ["hello 32", "hello 64"]);
        assert_eq!(emu.next_picture(&mut source), (64, 32), "the resent frame is the new picture");
    }

    #[test]
    fn frames_held_back_after_a_rotation_are_counted_as_skipped() {
        let landscape = (128, 64);
        let emu = Emu::new(true, vec![(PHONE, PHONE), (landscape, landscape), (landscape, landscape)]);
        let mut source = AndroidSource::connect_fitted(&emu.dir, 64, Some(PHONE)).expect("connect");
        emu.next_picture(&mut source);
        assert_eq!(source.take_unreported(), 0);
        emu.next_picture(&mut source);
        assert_eq!(source.take_unreported(), 1, "the held frame is counted as skipped");
        emu.hellos_at_least(2);
        emu.next_picture(&mut source);
        assert_eq!(source.take_unreported(), 0, "and only once");
    }

    #[test]
    fn an_emu_that_ignores_the_request_after_a_rotation_gets_its_frames_through_in_the_end() {
        let landscape = (128, 64);
        let mut script = vec![(PHONE, PHONE)];
        script.extend(vec![(landscape, landscape); HOLD_FRAMES as usize + 2]);
        let emu = Emu::new(false, script);
        let mut source = AndroidSource::connect_fitted(&emu.dir, 64, Some(PHONE)).expect("connect");
        assert_eq!(emu.next_picture(&mut source), PHONE);
        let sizes: Vec<_> = (0..HOLD_FRAMES + 2).map(|_| emu.next_picture(&mut source)).collect();
        assert_eq!(sizes.last(), Some(&landscape), "frames flow again after {HOLD_FRAMES} held");
        assert_eq!(sizes.iter().filter(|&&s| s == PHONE).count(), HOLD_FRAMES as usize + 1, "held ones keep the last picture");
    }

    #[test]
    fn the_newest_frame_is_the_picture_with_its_capture_time_and_count_of_skipped_ones() {
        let display = fake_display("source", (6, 4), 4, Duration::ZERO);
        let mut source = AndroidSource::connect(&display.dir, 0).expect("connect");
        assert!(source.picture().expect("picture").is_none(), "nothing before the first frame: the display holds them back");
        display.go();
        let first = source.wait(Instant::now() + Duration::from_secs(10)).expect("wait").expect("a frame");
        let reported = source.reader.seqs().1.expect("wait reported one");
        wait_for_seq(&source.reader, 3);
        let picture = source.picture().expect("picture").expect("a picture");
        assert_eq!((picture.size(), picture.stride, picture.order), ((6, 4), 24, PixelOrder::Rgba));
        assert!(picture.data.iter().all(|&b| b == 3), "the newest of four, which holds byte 3");
        let captured = source.take_captured().expect("when it arrived");
        assert!(captured >= first && source.take_captured().is_none(), "once");
        assert_eq!(source.take_unreported(), 3 - reported, "the skipped frames");
        assert_eq!(source.take_unreported(), 0, "once");
        assert_eq!(source.device_size(), Some((6, 4)));
        // Asking again hands back the same picture until a newer frame arrives.
        assert!(source.picture().expect("picture").is_some());
        assert!(source.take_captured().is_none());
    }

    #[test]
    fn a_device_that_is_not_running_is_a_source_error() {
        let dir = std::env::temp_dir().join(format!("simee-encode-nodevice-{}", std::process::id()));
        assert!(matches!(AndroidSource::connect(&dir, 0), Err(Error::Source(_))));
    }
}
