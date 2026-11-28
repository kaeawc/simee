//! The frame source on Linux: emu's display socket and frame ring, the same stream the macOS
//! [`crate::AndroidSource`] reads, as pictures for a software encoder. emu's frames are RGBA rows;
//! the picture is that frame itself (no copy: the buffer the reader filled is the buffer the encoder
//! reads, and the previous picture's buffer goes back to the reader).

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

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
}

impl AndroidSource {
    /// Connects to the AVD in `avd_dir` and asks for frames at most `max_width` pixels wide (0:
    /// full size; emu shrinks by a whole factor, `ceil(width / max_width)`).
    pub fn connect(avd_dir: &Path, max_width: u32) -> Result<AndroidSource> {
        Ok(AndroidSource { reader: FrameReader::connect(avd_dir, max_width)?, current: None, taken_at: None, unreported: 0 })
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

impl Source for AndroidSource {
    type Picture = Picture;

    fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
        self.reader.wait(deadline)
    }

    fn picture(&mut self) -> Result<Option<&Picture>> {
        if let Some(frame) = self.reader.take() {
            self.unreported = frame.unreported;
            self.taken_at = Some(frame.at);
            let (width, height) = (frame.width, frame.height);
            if frame.rgba.len() != width as usize * height as usize * 4 {
                return Err(Error::Source(format!("a {width}x{height} frame of {} bytes", frame.rgba.len())));
            }
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
    use crate::android_frames::tests::{fake_display, wait_for_seq};
    use std::time::Duration;

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
