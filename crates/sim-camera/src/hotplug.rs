//! Hot-plug: the host says something changed (inotify on `/dev` for `video*` nodes on Linux,
//! `AVCaptureDeviceWasConnected` / `WasDisconnected` on macOS), the signals are debounced (a USB
//! camera adds two nodes, and udev changes their mode after creating them), then the cameras are
//! listed again and the difference becomes [`Event`]s on the hub's event stream
//! ([`crate::Hub::events`]).

use std::time::{Duration, Instant};

use crate::catalog::{self, HostCamera};
use crate::error::{Error, Result};

/// Something a hub's event stream reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A camera appeared.
    Added(HostCamera),
    /// A camera went away.
    Removed(HostCamera),
    /// A running capture lost its source (the camera was unplugged mid-capture); its subscribers
    /// get `error` as their next receive.
    SourceMissing { source: String, error: Error },
}

impl From<catalog::Change> for Event {
    fn from(change: catalog::Change) -> Event {
        match change {
            catalog::Change::Added(c) => Event::Added(c),
            catalog::Change::Removed(c) => Event::Removed(c),
        }
    }
}

/// Raw "the cameras may have changed" signals from the host.
pub trait Signals: Send {
    /// Waits up to `timeout`; `Ok(true)` when a signal came.
    fn wait(&mut self, timeout: Duration) -> Result<bool>;
}

/// Where a hub's hot-plug events come from: the host, or fakes in tests.
pub trait HotPlug: Send + Sync {
    /// The cameras now.
    fn cameras(&self) -> Result<Vec<HostCamera>>;
    /// Starts watching for changes.
    fn watch(&self) -> Result<Box<dyn Signals>>;
}

/// The host's cameras and its device notifications.
pub struct HostHotPlug;

impl HotPlug for HostHotPlug {
    fn cameras(&self) -> Result<Vec<HostCamera>> {
        catalog::host_cameras()
    }

    fn watch(&self) -> Result<Box<dyn Signals>> {
        #[cfg(target_os = "macos")]
        {
            crate::avf::watch()
        }
        #[cfg(target_os = "linux")]
        {
            crate::v4l2::watch()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Err(Error::Unsupported("camera hot-plug events aren't available on this host".into()))
        }
    }
}

/// How long the signals must stop before the cameras are listed again.
pub(crate) const QUIET: Duration = Duration::from_millis(300);
/// The longest a steady stream of signals can hold a listing back.
pub(crate) const MAX_DELAY: Duration = Duration::from_secs(2);

/// Coalesces a burst of signals into one listing: due `quiet` after the last signal, or `max`
/// after the first, whichever is sooner.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Debounce {
    quiet: Duration,
    max: Duration,
    first: Option<Instant>,
    due: Option<Instant>,
}

impl Debounce {
    pub(crate) fn new(quiet: Duration, max: Duration) -> Debounce {
        Debounce { quiet, max, first: None, due: None }
    }

    pub(crate) fn signal(&mut self, now: Instant) {
        let first = *self.first.get_or_insert(now);
        self.due = Some((now + self.quiet).min(first + self.max));
    }

    /// Whether a listing is due now; clears the burst when it is.
    pub(crate) fn take(&mut self, now: Instant) -> bool {
        if self.due.is_some_and(|due| now >= due) {
            (self.first, self.due) = (None, None);
            return true;
        }
        false
    }

    /// How long to wait for the next signal: until the listing is due, or `idle` with none pending.
    pub(crate) fn timeout(&self, now: Instant, idle: Duration) -> Duration {
        self.due.map_or(idle, |due| due.saturating_duration_since(now).min(idle))
    }
}

#[cfg(any(target_os = "linux", test))]
/// Whether a `/dev` entry is a V4L2 node: `video` and a number.
pub(crate) fn is_video_node(name: &[u8]) -> bool {
    name.strip_prefix(b"video").is_some_and(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit))
}

#[cfg(any(target_os = "linux", test))]
/// `IN_Q_OVERFLOW`: events were lost, so anything may have changed.
const IN_Q_OVERFLOW: u32 = 0x4000;
#[cfg(any(target_os = "linux", test))]
/// `sizeof(struct inotify_event)` before its name.
const INOTIFY_HEADER: usize = 16;

#[cfg(any(target_os = "linux", test))]
/// Whether a buffer `read` from an inotify descriptor holds an event for a `video*` node (or an
/// overflow). Each event is `{ i32 wd; u32 mask; u32 cookie; u32 len; char name[len]; }`, the name
/// NUL-padded; a truncated tail is ignored.
pub(crate) fn inotify_names_video(mut buf: &[u8]) -> bool {
    let word = |b: &[u8], i: usize| u32::from_ne_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    while buf.len() >= INOTIFY_HEADER {
        let (mask, len) = (word(buf, 4), word(buf, 12) as usize);
        let Some(name) = buf.get(INOTIFY_HEADER..INOTIFY_HEADER + len) else { return false };
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
        if mask & IN_Q_OVERFLOW != 0 || is_video_node(name) {
            return true;
        }
        buf = &buf[INOTIFY_HEADER + len..];
    }
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn a_burst_of_signals_is_one_listing_after_it_goes_quiet() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut d = Debounce::new(Duration::from_millis(300), Duration::from_secs(2));
        assert!(!d.take(ms(0)), "nothing pending");
        assert_eq!(d.timeout(ms(0), Duration::from_millis(500)), Duration::from_millis(500));
        d.signal(ms(0));
        d.signal(ms(100)); // the metadata node
        d.signal(ms(250)); // udev's chmod
        assert!(!d.take(ms(500)), "still within 300 ms of the last signal");
        assert_eq!(d.timeout(ms(500), Duration::from_secs(1)), Duration::from_millis(50));
        assert!(d.take(ms(550)));
        assert!(!d.take(ms(5000)), "one listing per burst");
    }

    #[test]
    fn a_steady_stream_of_signals_still_lists_within_the_cap() {
        let t0 = Instant::now();
        let mut d = Debounce::new(Duration::from_millis(300), Duration::from_secs(2));
        let mut listed = Vec::new();
        for n in (0..4000).step_by(100) {
            let now = t0 + Duration::from_millis(n);
            if d.take(now) {
                listed.push(n);
            }
            d.signal(now);
        }
        assert_eq!(listed, vec![2000], "at the cap, not never");
    }

    #[test]
    fn only_video_nodes_count() {
        for yes in [&b"video0"[..], b"video12"] {
            assert!(is_video_node(yes), "{yes:?}");
        }
        for no in [&b"video"[..], b"videox", b"video0p", b"media0", b"v4l-subdev0", b"vhci", b""] {
            assert!(!is_video_node(no), "{no:?}");
        }
    }

    /// One `struct inotify_event`, its name NUL-padded to `pad` bytes.
    fn event(mask: u32, name: &str, pad: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&1i32.to_ne_bytes());
        out.extend_from_slice(&mask.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&(pad as u32).to_ne_bytes());
        let mut n = name.as_bytes().to_vec();
        n.resize(pad, 0);
        out.extend_from_slice(&n);
        out
    }

    #[test]
    fn inotify_buffers_are_parsed_for_video_nodes() {
        const IN_CREATE: u32 = 0x100;
        let other = [event(IN_CREATE, "ttyUSB0", 16), event(IN_CREATE, "media0", 16)].concat();
        assert!(!inotify_names_video(&other));
        let with_video = [other.clone(), event(IN_CREATE, "video2", 16)].concat();
        assert!(inotify_names_video(&with_video));
        assert!(inotify_names_video(&event(IN_Q_OVERFLOW, "", 0)), "an overflow could hide anything");
        // A name exactly filling its padding (no NUL), and a truncated tail.
        assert!(inotify_names_video(&event(IN_CREATE, "video10", 7)));
        let truncated = &with_video[..with_video.len() - 4];
        assert!(!inotify_names_video(truncated));
        assert!(!inotify_names_video(&[]));
    }

    #[test]
    fn changes_become_events() {
        let cam = crate::catalog::tests::camera("a", "A");
        assert_eq!(Event::from(catalog::Change::Added(cam.clone())), Event::Added(cam.clone()));
        assert_eq!(Event::from(catalog::Change::Removed(cam.clone())), Event::Removed(cam));
    }
}
