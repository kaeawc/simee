//! `device:<udid or name>[:x,y,w,h]`: another simee device's live screen as a camera, optionally
//! cropped to a region of it, so one device can scan a QR code another one shows.
//!
//! A reader thread posts the source device's frames to a [`Screen`] as the device presents them,
//! and a [`DeviceSource`] turns the newest one into camera frames. The readers (an Android device
//! through emu's display socket, a simulator through its framebuffer surface) link that platform's
//! stack, so they live in `sim-api`, which the simee server runs: this crate never links them, and
//! neither does emu, which reads another device's screen as a `feed:` the server serves it
//! ([`crate::feed`]). Here a `device:` source opens to [`served_by_simee`]'s error.
//!
//! Each subscriber size is drawn in one pass from the device frame: the region cropped, the result
//! scaled to fill the size (aspect kept, overflow cropped from both sides, nearest neighbour) and
//! swizzled to BGRA. Pacing follows the device: a capture tick takes a frame the device presented
//! since the last one, waiting up to one tick for it, and repeats the last frame (shared, not
//! redrawn) while the screen is still. A region the screen no longer covers (the device rotated)
//! draws black, said once, until it is on the screen again: the capture never fails over it, so
//! nothing reconnects. The source device going away (shut down, its display closed) is
//! [`Error::Missing`], which the hub reports as `SourceMissing`.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::id::SourceSpec;
use crate::source::Source;

/// A rectangle of a device's screen, in its own pixels (what `simee screenshot` gives).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

/// A parsed `device:` source: the device as written, and the region to crop to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceSpec {
    /// An AVD name, or a simulator's UDID or name.
    pub device: String,
    pub region: Option<Region>,
}

const EXAMPLE: &str = "device:Pixel_9 or device:<udid>:0,0,540,960";

impl DeviceSpec {
    /// Parses what follows `device:`. A last `:x,y,w,h` (four numbers) is the region; anything
    /// else after a `:` belongs to the name.
    pub fn parse(rest: &str) -> Result<DeviceSpec> {
        let rest = rest.trim();
        let (device, region) = match rest.rsplit_once(':') {
            Some((device, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == ',' || c == ' ') => {
                (device.trim(), Some(parse_region(tail)?))
            }
            _ => (rest, None),
        };
        if device.is_empty() {
            return Err(Error::Invalid(format!("device: needs a device's UDID or name, as in {EXAMPLE}")));
        }
        Ok(DeviceSpec { device: device.to_string(), region })
    }

    /// Whether this names the device that goes by one of `names` (its UDID or AVD name, its
    /// name), ignoring case.
    pub fn names(&self, names: &[&str]) -> bool {
        names.iter().map(|n| n.trim()).any(|n| !n.is_empty() && n.eq_ignore_ascii_case(&self.device))
    }

    /// Whether this is `device:booted`, which names whichever simulator is booted: resolve it
    /// before comparing it with anything.
    pub fn is_booted(&self) -> bool {
        self.device.eq_ignore_ascii_case("booted")
    }
}

/// What opening a `device:` source says outside the simee server: who captures it.
pub fn served_by_simee(spec: &DeviceSpec) -> Error {
    Error::Unsupported(format!(
        "device:{}: another device's screen is captured by the simee server (`simee serve`), not here; link it with `simee camera \
         link`, which serves it to this device as a feed: source",
        spec.device
    ))
}

/// Checks `region` against a device whose screen is `screen` now: a region off the screen is an
/// error naming the screen's size; one partly off is clipped to it (fine).
pub fn check_region(name: &str, region: Option<Region>, screen: (u32, u32)) -> Result<()> {
    window(region, screen, screen).map(drop).map_err(|e| Error::Invalid(format!("device:{name}: {e}")))
}

/// The frame width to ask a `screen`-sized device for (its frames come downscaled by a whole
/// factor to at most this wide) so that `region` (the whole screen without one) still fills a
/// `spec`-sized camera frame at its own resolution or better: the smallest such frame, never wider
/// than the screen.
pub fn needed_width(region: Option<Region>, screen: (u32, u32), spec: FrameSpec) -> u32 {
    let (sw, sh) = (screen.0.max(1), screen.1.max(1));
    let r = window(region, (sw, sh), (sw, sh)).unwrap_or(Region { x: 0, y: 0, width: sw, height: sh });
    let (rw, rh, dw, dh) = (u64::from(r.width), u64::from(r.height), u64::from(spec.width), u64::from(spec.height));
    // The part of the region the camera frame shows (aspect kept, the overflow cropped).
    let visible = if rw * dh > dw * rh { (rh * dw / dh).max(1) } else { rw };
    // The screen width at which that part is `dw` wide, as a whole downscale factor of the screen.
    let wanted = (u64::from(sw) * dw).div_ceil(visible).min(u64::from(sw));
    let factor = (u64::from(sw) / wanted.max(1)).max(1);
    u64::from(sw).div_ceil(factor) as u32
}

impl fmt::Display for DeviceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.region {
            Some(r) => write!(f, "{}:{r}", self.device),
            None => f.write_str(&self.device),
        }
    }
}

fn parse_region(text: &str) -> Result<Region> {
    let bad = || Error::Invalid(format!("device: region {text:?} is not x,y,width,height in the device's pixels, as in {EXAMPLE}"));
    let n: Vec<u32> = text.split(',').map(|p| p.trim().parse::<u32>()).collect::<std::result::Result<_, _>>().map_err(|_| bad())?;
    let [x, y, width, height] = n[..] else { return Err(bad()) };
    if width == 0 || height == 0 {
        return Err(Error::Invalid(format!("device: region {text:?} is empty; give it a width and a height")));
    }
    Ok(Region { x, y, width, height })
}

/// Refuses linking a device's camera to its own screen: the feed would film itself, each frame
/// the last one again, smaller. `target` holds the names the target device goes by (its UDID or
/// AVD name, and its name). Anything that isn't a `device:` source passes.
pub fn refuse_feedback(source: &SourceSpec, target: &[&str]) -> Result<()> {
    match source {
        SourceSpec::Device(d) if d.names(target) => Err(Error::Invalid(format!(
            "device:{}: a device's camera can't show its own screen (a feedback loop); link another device's screen",
            d.device
        ))),
        _ => Ok(()),
    }
}

/// One frame of a device's screen as the device presented it: RGBA rows, top row first.
#[derive(Clone, Debug)]
pub struct Shot {
    /// Counts up from 0 with every frame posted to the [`Screen`].
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// The device's own screen size, which regions are given in (the frame may be smaller).
    pub screen: (u32, u32),
    pub rgba: Arc<Vec<u8>>,
}

/// The newest frame of a device's screen: what a backend's reader thread (or a test) posts, and a
/// [`DeviceSource`] reads.
///
/// It also carries what the capture needs of the device ([`Screen::want`]): every camera size and
/// the highest rate its subscribers asked for, so a reader started for the first subscriber asks
/// the device again when a larger one joins (one capture serves the largest, scaled down for the
/// others).
#[derive(Default)]
pub struct Screen {
    latest: Mutex<Latest>,
    arrived: Condvar,
    wanted: Mutex<Wanted>,
}

/// Called with what the capture needs now, each time that grows.
type Asker = Arc<dyn Fn(&[FrameSpec]) + Send + Sync>;

#[derive(Default)]
struct Wanted {
    /// The sizes asked for (each at the highest rate asked), at most [`SIZES`] of them.
    specs: Vec<FrameSpec>,
    asker: Option<Asker>,
}

#[derive(Default)]
struct Latest {
    shot: Option<Shot>,
    next_seq: u64,
    gone: Option<String>,
}

impl Screen {
    fn lock(&self) -> MutexGuard<'_, Latest> {
        self.latest.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Posts a `width`x`height` RGBA frame of a `screen`-sized display. Returns a buffer to read
    /// the next frame into: the replaced frame's, when no source holds it any more.
    pub fn post(&self, width: u32, height: u32, screen: (u32, u32), rgba: Vec<u8>) -> Vec<u8> {
        let mut latest = self.lock();
        let seq = latest.next_seq;
        latest.next_seq += 1;
        let old = latest.shot.replace(Shot { seq, width, height, screen, rgba: Arc::new(rgba) });
        drop(latest);
        self.arrived.notify_all();
        let mut spare = old.and_then(|s| Arc::try_unwrap(s.rgba).ok()).unwrap_or_default();
        spare.clear();
        spare
    }

    /// Adds `spec` to what the capture needs: a size not asked for yet, or a higher rate (every
    /// size is asked at the highest). When that grows, the reader's asker ([`Screen::on_want`]) is
    /// called with all of it, outside the lock.
    pub fn want(&self, spec: FrameSpec) {
        let (specs, asker) = {
            let mut wanted = self.wanted.lock().unwrap_or_else(|p| p.into_inner());
            let fps = wanted.specs.iter().map(|s| s.fps).max().unwrap_or(0).max(spec.fps);
            let known = wanted.specs.iter().any(|s| (s.width, s.height) == (spec.width, spec.height));
            let added = !known && wanted.specs.len() < SIZES;
            if !added && wanted.specs.first().is_none_or(|s| s.fps >= fps) {
                return;
            }
            if added {
                wanted.specs.push(spec);
            }
            for s in &mut wanted.specs {
                s.fps = fps;
            }
            (wanted.specs.clone(), wanted.asker.clone())
        };
        if let Some(ask) = asker {
            ask(&specs);
        }
    }

    /// What the capture needs now: each size asked for, at the highest rate asked.
    pub fn wanted(&self) -> Vec<FrameSpec> {
        self.wanted.lock().unwrap_or_else(|p| p.into_inner()).specs.clone()
    }

    /// Sets what a reader does when the capture needs more ([`Screen::want`]): ask the device for
    /// a larger or faster stream.
    pub fn on_want(&self, ask: impl Fn(&[FrameSpec]) + Send + Sync + 'static) {
        self.wanted.lock().unwrap_or_else(|p| p.into_inner()).asker = Some(Arc::new(ask));
    }

    /// The device went away: every read from now on is [`Error::Missing`] with `why`.
    pub fn close(&self, why: impl Into<String>) {
        self.lock().gone = Some(why.into());
        self.arrived.notify_all();
    }

    /// The newest frame after frame `after` (any frame, with `None`), waiting up to `timeout` for
    /// one: `Ok(None)` when none came.
    fn newer(&self, after: Option<u64>, timeout: Duration) -> Result<Option<Shot>> {
        let deadline = Instant::now() + timeout;
        let mut latest = self.lock();
        loop {
            if let Some(why) = &latest.gone {
                return Err(Error::Missing(why.clone()));
            }
            if let Some(shot) = latest.shot.as_ref().filter(|s| after.is_none_or(|a| s.seq > a)) {
                return Ok(Some(shot.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            latest = self.arrived.wait_timeout(latest, deadline - now).map_or_else(|p| p.into_inner().0, |(g, _)| g);
        }
    }
}

/// How long a capture waits for the source device's first frame.
const FIRST_FRAME: Duration = Duration::from_secs(5);

/// Sizes drawn from one device frame at once; more distinct sizes than this start over.
const SIZES: usize = 8;

/// What keeps a backend's reader running; dropping it stops the reader.
pub type Reader = Box<dyn Send>;

/// A camera frame drawn from the current device frame, by its size.
type Drawn = ((u32, u32), Arc<[u8]>);

/// A device's screen as a camera source.
pub struct DeviceSource {
    name: String,
    region: Option<Region>,
    screen: Arc<Screen>,
    first_frame: Duration,
    /// The device frame the camera frames are drawn from now.
    shot: Option<Shot>,
    /// The capture tick that last waited for a device frame: the other sizes of that tick are
    /// drawn from the same one.
    waited: Option<u64>,
    drawn: Vec<Drawn>,
    /// The region is off the screen now, and that was said.
    off_screen: bool,
    /// The subscriber specs passed on to the screen ([`Screen::want`]).
    asked: Vec<FrameSpec>,
    _reader: Option<Reader>,
}

impl DeviceSource {
    /// A source over `screen`, which something else posts `name`'s frames to (a reader thread,
    /// or a test's fake).
    pub fn new(name: &str, region: Option<Region>, screen: Arc<Screen>) -> DeviceSource {
        DeviceSource {
            name: name.to_string(),
            region,
            screen,
            first_frame: FIRST_FRAME,
            shot: None,
            waited: None,
            drawn: Vec::new(),
            off_screen: false,
            asked: Vec::new(),
            _reader: None,
        }
    }

    /// Keeps `reader` (whatever posts to the screen) for the source's life: dropping the source
    /// drops it, which stops it.
    pub fn reading(mut self, reader: Reader) -> DeviceSource {
        self._reader = Some(reader);
        self
    }

    /// The window of `shot` to draw: `None` (black) while the region is off the screen, said once
    /// each time it goes off.
    fn window_of(&mut self, shot: &Shot) -> Option<Region> {
        match window(self.region, (shot.width, shot.height), shot.screen) {
            Ok(w) => {
                self.off_screen = false;
                Some(w)
            }
            Err(e) => {
                if !std::mem::replace(&mut self.off_screen, true) {
                    eprintln!("simee: camera device:{}: {e} (rotated?); the camera shows black until it is on the screen again", self.name);
                }
                None
            }
        }
    }
}

impl Source for DeviceSource {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        // A subscriber size (or rate) not seen yet: the reader may need to ask the device for more.
        if !self.asked.contains(&spec) {
            if self.asked.len() >= SIZES * 4 {
                self.asked.clear();
            }
            self.asked.push(spec);
            self.screen.want(spec);
        }
        if self.waited != Some(tick.seq) {
            self.waited = Some(tick.seq);
            let timeout = if self.shot.is_some() { spec.interval() } else { self.first_frame };
            if let Some(shot) = self.screen.newer(self.shot.as_ref().map(|s| s.seq), timeout)? {
                self.shot = Some(shot);
                self.drawn.clear();
            }
        }
        let Some(shot) = self.shot.clone() else {
            return Err(Error::Capture(format!("no frames from device {} in {} s", self.name, self.first_frame.as_secs())));
        };
        let size = (spec.width, spec.height);
        let bgra = match self.drawn.iter().find(|(s, _)| *s == size) {
            Some((_, bgra)) => bgra.clone(),
            None => {
                let bgra: Arc<[u8]> = match self.window_of(&shot) {
                    Some(window) => crop_fill(&shot.rgba, shot.width, shot.height, window, spec.width, spec.height)?.into(),
                    None => black(spec.width, spec.height).into(),
                };
                if self.drawn.len() >= SIZES {
                    self.drawn.clear();
                }
                self.drawn.push((size, bgra.clone()));
                bgra
            }
        };
        Ok(CameraFrame { width: spec.width, height: spec.height, bgra, tick })
    }

    /// Each subscriber size is drawn from the device frame (one pass: crop, scale, swizzle)
    /// rather than fitted from another size's.
    fn synthetic(&self) -> bool {
        true
    }
}

/// The part of a `frame`-sized picture that `region` covers (the whole picture without one):
/// `region` is in the device's `screen` pixels, clamped to the screen, then mapped to the frame's
/// (a frame smaller than the screen was downscaled by the device).
pub fn window(region: Option<Region>, frame: (u32, u32), screen: (u32, u32)) -> std::result::Result<Region, String> {
    let (fw, fh) = frame;
    let (sw, sh) = if screen.0 == 0 || screen.1 == 0 { frame } else { screen };
    let Some(r) = region else { return Ok(Region { x: 0, y: 0, width: fw, height: fh }) };
    if r.x >= sw || r.y >= sh {
        return Err(format!("region {r} is outside the {sw}x{sh} screen"));
    }
    let to = |v: u32, from: u32, into: u32| (u64::from(v) * u64::from(into) / u64::from(from)) as u32;
    let (x1, y1) = (r.x.saturating_add(r.width).min(sw), r.y.saturating_add(r.height).min(sh));
    let (x, y) = (to(r.x, sw, fw).min(fw.saturating_sub(1)), to(r.y, sh, fh).min(fh.saturating_sub(1)));
    let width = to(x1, sw, fw).saturating_sub(x).max(1);
    let height = to(y1, sh, fh).saturating_sub(y).max(1);
    Ok(Region { x, y, width, height })
}

/// An opaque black BGRA frame.
fn black(width: u32, height: u32) -> Vec<u8> {
    [0, 0, 0, 255].repeat(width as usize * height as usize)
}

/// Draws `window` of an RGBA picture into a `dst_w`x`dst_h` BGRA frame the way a camera fills
/// its sensor: aspect kept, the whole frame covered, the overflow cropped equally from both sides
/// (nearest neighbour), every pixel opaque.
pub fn crop_fill(src: &[u8], src_w: u32, src_h: u32, window: Region, dst_w: u32, dst_h: u32) -> Result<Vec<u8>> {
    let (sw, sh) = (src_w as usize, src_h as usize);
    if src.len() < sw * sh * 4
        || window.width == 0
        || window.height == 0
        || window.x.saturating_add(window.width) > src_w
        || window.y.saturating_add(window.height) > src_h
    {
        return Err(Error::Capture(format!("a {src_w}x{src_h} device frame of {} bytes can't be cropped to {window}", src.len())));
    }
    let (ww, wh, dw, dh) = (u64::from(window.width), u64::from(window.height), u64::from(dst_w), u64::from(dst_h));
    let mut out = vec![0u8; dst_w as usize * dst_h as usize * 4];
    if dw == 0 || dh == 0 {
        return Ok(out);
    }
    let (vw, vh) = if ww * dh > dw * wh { ((wh * dw / dh).max(1), wh) } else { (ww, (ww * dh / dw).max(1)) };
    let (x0, y0) = (u64::from(window.x) + (ww - vw) / 2, u64::from(window.y) + (wh - vh) / 2);
    let columns: Vec<usize> = (0..dw).map(|x| ((x0 + x * vw / dw) * 4) as usize).collect();
    for (y, row) in out.chunks_exact_mut(dst_w as usize * 4).enumerate() {
        let sy = (y0 + y as u64 * vh / dh) as usize;
        let src_row = &src[sy * sw * 4..][..sw * 4];
        for (px, &sx) in row.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            *px = [src_row[sx + 2], src_row[sx + 1], src_row[sx], 255];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotplug::{Event, HotPlug, Signals};
    use crate::hub::Hub;
    use crate::source::Opener;
    use crate::{CameraFrame, HostCamera};
    use std::thread;

    fn spec(id: &str) -> DeviceSpec {
        match SourceSpec::parse(id) {
            Ok(SourceSpec::Device(d)) => d,
            other => panic!("{id}: {other:?}"),
        }
    }

    #[test]
    fn device_ids_parse_with_and_without_a_region_and_round_trip() {
        let r = |x, y, width, height| Some(Region { x, y, width, height });
        let cases = [
            ("device:Pixel_9", "Pixel_9", None),
            ("device:Pixel_9:0,100,540,960", "Pixel_9", r(0, 100, 540, 960)),
            ("device:5A1B2C3D-0000-4000-8000-000000000001:10,20,30,40", "5A1B2C3D-0000-4000-8000-000000000001", r(10, 20, 30, 40)),
            ("device:iPhone 16 Pro", "iPhone 16 Pro", None),
            ("device:Jo's phone: work", "Jo's phone: work", None),
        ];
        for (id, device, region) in cases {
            let d = spec(id);
            assert_eq!((d.device.as_str(), d.region), (device, region), "{id}");
            assert_eq!(SourceSpec::Device(d.clone()).to_string(), id);
            assert_eq!(SourceSpec::parse(&SourceSpec::Device(d).to_string()).map(|s| s.kind()), Ok("device"));
        }
        assert_eq!(spec("DEVICE: Pixel : 1, 2, 3, 4 ").region, r(1, 2, 3, 4));
    }

    #[test]
    fn bad_device_ids_explain_themselves() {
        for (id, want) in [
            ("device:", "needs a device's UDID or name"),
            ("device::0,0,10,10", "needs a device's UDID or name"),
            ("device:Pixel:0,0,10", "not x,y,width,height"),
            ("device:Pixel:0,0,0,10", "is empty"),
            ("device:Pixel:1,2,3,99999999999", "not x,y,width,height"),
        ] {
            let err = SourceSpec::parse(id).expect_err(id).to_string();
            assert!(err.contains(want), "{id}: {err}");
        }
    }

    #[test]
    fn a_device_is_refused_as_its_own_camera() {
        let target = ["5A1B2C3D-0000-4000-8000-000000000001", "iPhone 16"];
        for id in ["device:5a1b2c3d-0000-4000-8000-000000000001", "device:iPhone 16:0,0,10,10", "device:IPHONE 16"] {
            let err = refuse_feedback(&SourceSpec::parse(id).unwrap_or_else(|e| panic!("{e}")), &target).expect_err(id);
            assert!(err.to_string().contains("feedback loop"), "{err}");
        }
        assert_eq!(refuse_feedback(&SourceSpec::parse("device:Pixel_9").unwrap_or_else(|e| panic!("{e}")), &target), Ok(()));
        assert_eq!(refuse_feedback(&SourceSpec::Qr("iPhone 16".into()), &target), Ok(()));
        assert_eq!(refuse_feedback(&SourceSpec::Device(spec("device:x")), &["", " "]), Ok(()), "a blank name names nothing");
    }

    #[test]
    fn a_region_is_clamped_to_the_screen_and_mapped_to_the_frame() {
        let reg = |x, y, width, height| Region { x, y, width, height };
        assert_eq!(window(None, (1080, 1920), (1080, 1920)), Ok(reg(0, 0, 1080, 1920)));
        assert_eq!(window(Some(reg(100, 200, 300, 400)), (1080, 1920), (1080, 1920)), Ok(reg(100, 200, 300, 400)));
        // Past the right and bottom edges: what is on the screen.
        assert_eq!(window(Some(reg(900, 1800, 500, 500)), (1080, 1920), (1080, 1920)), Ok(reg(900, 1800, 180, 120)));
        // A frame the device sent at half size: the region in screen pixels lands on half.
        assert_eq!(window(Some(reg(540, 960, 540, 960)), (540, 960), (1080, 1920)), Ok(reg(270, 480, 270, 480)));
        // No screen size: the frame's own.
        assert_eq!(window(Some(reg(1, 1, 2, 2)), (4, 4), (0, 0)), Ok(reg(1, 1, 2, 2)));
        let err = window(Some(reg(1080, 0, 10, 10)), (1080, 1920), (1080, 1920)).expect_err("outside");
        assert!(err.contains("outside the 1080x1920 screen"), "{err}");
    }

    /// A 4x4 RGBA picture whose pixel (x, y) is (x, y, 7, 9): each output pixel names its source.
    fn grid(w: u32, h: u32) -> Vec<u8> {
        (0..h).flat_map(|y| (0..w).flat_map(move |x| [x as u8, y as u8, 7, 9])).collect()
    }

    /// The source (x, y) of each output pixel, from its BGRA (7, y, x, 255).
    fn sources(bgra: &[u8]) -> Vec<(u8, u8)> {
        bgra.as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                assert_eq!((p[0], p[3]), (7, 255), "swizzled to BGRA, opaque");
                (p[2], p[1])
            })
            .collect()
    }

    #[test]
    fn crop_fill_crops_scales_and_swizzles_in_one_pass() {
        let src = grid(4, 4);
        let whole = Region { x: 0, y: 0, width: 4, height: 4 };
        // Same size: every pixel, in place.
        let same = sources(&crop_fill(&src, 4, 4, whole, 4, 4).unwrap_or_default());
        assert_eq!(same[5], (1, 1));
        assert_eq!(same[15], (3, 3));
        // Down by two: every other pixel.
        assert_eq!(sources(&crop_fill(&src, 4, 4, whole, 2, 2).unwrap_or_default()), [(0, 0), (2, 0), (0, 2), (2, 2)]);
        // A region, scaled up: its pixels repeated.
        let region = Region { x: 2, y: 1, width: 2, height: 2 };
        let up = sources(&crop_fill(&src, 4, 4, region, 4, 4).unwrap_or_default());
        assert_eq!(&up[..4], &[(2, 1), (2, 1), (3, 1), (3, 1)]);
        assert_eq!(up[15], (3, 2));
        // A wide output from a square: the middle rows, cropped equally top and bottom.
        assert_eq!(sources(&crop_fill(&src, 4, 4, whole, 4, 2).unwrap_or_default())[..4], [(0, 1), (1, 1), (2, 1), (3, 1)]);
        // A tall output from a square region: its middle columns.
        assert_eq!(sources(&crop_fill(&src, 4, 4, whole, 2, 4).unwrap_or_default())[..2], [(1, 0), (2, 0)]);
        // A window off the frame, or a short frame, is an error, not garbage.
        assert!(crop_fill(&src, 4, 4, Region { x: 3, y: 0, width: 2, height: 1 }, 2, 2).is_err());
        assert!(crop_fill(&src[..20], 4, 4, whole, 2, 2).is_err());
    }

    fn tick(seq: u64) -> Tick {
        Tick { seq, ..Tick::start() }
    }

    fn fps(width: u32, height: u32, fps: u32) -> FrameSpec {
        FrameSpec::new(width, height, fps).unwrap_or_default()
    }

    fn solid(screen: &Screen, value: u8) {
        screen.post(64, 64, (64, 64), vec![value; 64 * 64 * 4]);
    }

    fn take(source: &mut DeviceSource, spec: FrameSpec, seq: u64) -> CameraFrame {
        source.frame(spec, tick(seq)).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn pacing_follows_the_device_frames_and_repeats_a_still_screen() {
        let screen = Arc::new(Screen::default());
        let mut source = DeviceSource::new("A", None, screen.clone());
        // A long tick (500 ms), so "when it comes, not a tick later" holds by a wide margin on a
        // loaded host.
        let s = fps(32, 32, 2);
        solid(&screen, 10);
        let first = take(&mut source, s, 0);
        assert_eq!(first.pixel(0, 0), [10, 10, 10, 255]);

        // Nothing new: the tick waits out one interval, then the same frame again, not redrawn.
        let started = Instant::now();
        let again = take(&mut source, s, 1);
        assert!(started.elapsed() >= s.interval() - Duration::from_millis(5), "{:?}", started.elapsed());
        assert!(Arc::ptr_eq(&again.bgra, &first.bgra));

        // A frame the device presents during the wait is taken when it comes, not a tick later.
        let poster = screen.clone();
        let post = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            solid(&poster, 20);
        });
        let started = Instant::now();
        let next = take(&mut source, s, 2);
        let waited = started.elapsed();
        post.join().ok();
        assert_eq!(next.pixel(0, 0), [20, 20, 20, 255]);
        assert!(waited >= Duration::from_millis(25) && waited < s.interval(), "{waited:?}");

        // The tick's other sizes are drawn from the same device frame, without another wait.
        let started = Instant::now();
        let other = take(&mut source, fps(48, 32, 2), 2);
        assert!(started.elapsed() < s.interval() / 2, "{:?}", started.elapsed());
        assert_eq!((other.width, other.height, other.pixel(47, 31)), (48, 32, [20, 20, 20, 255]));

        // Frames the device presented between ticks: the newest is taken at once.
        solid(&screen, 30);
        solid(&screen, 40);
        let started = Instant::now();
        assert_eq!(take(&mut source, s, 3).pixel(0, 0), [40, 40, 40, 255]);
        assert!(started.elapsed() < s.interval() / 2, "{:?}", started.elapsed());
    }

    #[test]
    fn frames_are_scaled_to_each_subscriber_size_from_the_region() {
        let screen = Arc::new(Screen::default());
        // A device at half size: 2x2 frames of a 4x4 screen; the region is the screen's right half.
        let region = Some(Region { x: 2, y: 0, width: 2, height: 4 });
        let mut source = DeviceSource::new("A", region, screen.clone());
        screen.post(2, 2, (4, 4), [[1, 0, 0, 255], [2, 0, 0, 255], [3, 0, 0, 255], [4, 0, 0, 255]].concat());
        let f = take(&mut source, fps(32, 64, 30), 0);
        assert_eq!((f.width, f.height), (32, 64));
        assert_eq!((f.pixel(0, 0), f.pixel(31, 63)), ([0, 0, 2, 255], [0, 0, 4, 255]), "the right column, top and bottom");
    }

    #[test]
    fn no_first_frame_is_a_capture_error_naming_the_device() {
        let mut source = DeviceSource::new("Pixel_9", None, Arc::new(Screen::default()));
        source.first_frame = Duration::from_millis(20);
        let err = source.frame(fps(32, 32, 30), tick(0)).expect_err("no frame");
        assert!(matches!(&err, Error::Capture(m) if m.contains("Pixel_9")), "{err:?}");
    }

    /// A portrait 64x128 screen whose rows below 80 are 200, above 80 are 10.
    fn portrait(screen: &Screen) {
        let rgba: Vec<u8> = (0..128u32).flat_map(|y| [if y >= 80 { 200 } else { 10 }; 64 * 4]).collect();
        screen.post(64, 128, (64, 128), rgba);
    }

    #[test]
    fn a_region_the_screen_stops_covering_draws_black_and_comes_back_without_failing() {
        let screen = Arc::new(Screen::default());
        // The bottom of a portrait screen: off the screen once it turns landscape (128x64).
        let mut source = DeviceSource::new("A", Some(Region { x: 0, y: 96, width: 64, height: 32 }), screen.clone());
        portrait(&screen);
        assert_eq!(take(&mut source, fps(32, 32, 30), 0).pixel(0, 0), [200, 200, 200, 255]);
        // Rotated: no error (an error would end the capture, and every reader would reconnect).
        screen.post(128, 64, (128, 64), vec![10; 128 * 64 * 4]);
        let f = take(&mut source, fps(32, 32, 30), 1);
        assert!(f.bgra.chunks(4).all(|p| p == [0, 0, 0, 255]), "black while off the screen");
        assert!(source.off_screen, "said once");
        let again = take(&mut source, fps(32, 32, 30), 2);
        assert!(Arc::ptr_eq(&again.bgra, &f.bgra) || again.bgra.chunks(4).all(|p| p == [0, 0, 0, 255]));
        // Back to portrait: the region again.
        portrait(&screen);
        assert_eq!(take(&mut source, fps(32, 32, 30), 3).pixel(0, 0), [200, 200, 200, 255]);
        assert!(!source.off_screen);
    }

    #[test]
    fn outside_the_server_a_device_source_says_who_captures_it() {
        let spec = SourceSpec::parse("device:Pixel_9:0,0,10,10").unwrap_or_else(|e| panic!("{e}"));
        let err = crate::HostOpener::new().open(&spec, FrameSpec::default()).err();
        assert!(matches!(&err, Some(Error::Unsupported(m)) if m.contains("device:Pixel_9") && m.contains("simee server")), "{err:?}");
    }

    #[test]
    fn a_region_is_checked_against_the_screen_it_will_crop() {
        let r = |x, y, width, height| Some(Region { x, y, width, height });
        assert_eq!(check_region("A", r(0, 1500, 1080, 600), (1080, 2400)), Ok(()));
        assert_eq!(check_region("A", r(1000, 0, 500, 10), (1080, 2400)), Ok(()), "partly off: clipped");
        assert_eq!(check_region("A", None, (1080, 2400)), Ok(()));
        for (region, screen) in [(r(0, 1500, 1080, 600), (2400, 1080)), (r(5000, 5000, 10, 10), (1080, 2400))] {
            let err = check_region("A", region, screen).expect_err("off the screen").to_string();
            assert!(err.contains("device:A") && err.contains(&format!("outside the {}x{} screen", screen.0, screen.1)), "{err}");
        }
    }

    #[test]
    fn the_frame_asked_of_a_device_is_the_smallest_that_keeps_the_cameras_resolution() {
        let vga = fps(640, 480, 30);
        // A whole 1080x2400 screen into 640x480: the visible part is 1080x810, so 640 wide does;
        // the widest whole downscale of 1080 that is at least 640 is 1080 itself.
        assert_eq!(needed_width(None, (1080, 2400), vga), 1080);
        assert_eq!(needed_width(None, (1080, 2400), fps(176, 144, 30)), 180, "a sixth: 180 >= 176");
        // A small region needs the screen at full size.
        assert_eq!(needed_width(Some(Region { x: 0, y: 1500, width: 540, height: 600 }), (1080, 2400), vga), 1080);
        // A big screen and a small camera: a quarter (1440 / 4 = 360 >= 352), never below it.
        assert_eq!(needed_width(None, (1440, 3120), fps(352, 288, 30)), 360);
        // Landscape into VGA: the visible part is 1440 of 2400 wide, so half (1200) keeps 720.
        assert_eq!(needed_width(None, (2400, 1080), vga), 1200);
        for (screen, spec) in [((1440, 3120), fps(352, 288, 30)), ((1080, 2400), fps(176, 144, 30)), ((2400, 1080), vga)] {
            let w = needed_width(None, screen, spec);
            assert!(w <= screen.0 && w >= spec.width.min(screen.0), "{screen:?} {spec:?}: {w}");
        }
    }

    #[test]
    fn a_frame_buffer_comes_back_once_nobody_holds_it() {
        let screen = Screen::default();
        assert!(screen.post(1, 1, (1, 1), vec![0; 4]).is_empty(), "nothing replaced");
        let spare = screen.post(1, 1, (1, 1), vec![0; 4]);
        assert!(spare.is_empty() && spare.capacity() >= 4, "the replaced frame's buffer, emptied");
        let held = screen.newer(None, Duration::ZERO).ok().flatten();
        assert_eq!(screen.post(1, 1, (1, 1), vec![0; 4]).capacity(), 0, "a source still holds it");
        drop(held);
    }

    /// Opens `device:` sources over screens the test posts to.
    struct Fakes(Arc<Mutex<Vec<Arc<Screen>>>>);

    impl Opener for Fakes {
        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> Result<Box<dyn Source>> {
            let SourceSpec::Device(d) = source else { return Err(Error::Invalid("not a device".into())) };
            let screen = Arc::new(Screen::default());
            self.0.lock().unwrap_or_else(|p| p.into_inner()).push(screen.clone());
            Ok(Box::new(DeviceSource::new(&d.device, d.region, screen)))
        }
    }

    struct NoCameras;

    struct Quiet;

    impl Signals for Quiet {
        fn wait(&mut self, timeout: Duration) -> Result<bool> {
            thread::sleep(timeout.min(Duration::from_millis(50)));
            Ok(false)
        }
    }

    impl HotPlug for NoCameras {
        fn cameras(&self) -> Result<Vec<HostCamera>> {
            Ok(Vec::new())
        }

        fn watch(&self) -> Result<Box<dyn Signals>> {
            Ok(Box::new(Quiet))
        }
    }

    #[test]
    fn a_source_device_going_away_is_source_missing() {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let hub = Hub::with_hotplug(Fakes(opened.clone()), NoCameras);
        let events = hub.events().unwrap_or_else(|e| panic!("{e}"));
        let source = SourceSpec::parse("device:A:0,0,32,32").unwrap_or_else(|e| panic!("{e}"));
        let sub = hub.subscribe(&source, fps(32, 32, 60)).unwrap_or_else(|e| panic!("{e}"));
        let screen = opened.lock().unwrap_or_else(|p| p.into_inner()).first().cloned().expect("opened");
        solid(&screen, 5);
        let frame = sub.recv_timeout(Duration::from_secs(5)).ok().flatten().expect("a frame");
        assert_eq!(frame.pixel(0, 0), [5, 5, 5, 255]);

        screen.close("A shut down");
        let failure = loop {
            match sub.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(_)) => continue,
                other => break other,
            }
        };
        let missing = Error::Missing("A shut down".into());
        assert_eq!(failure, Err(missing.clone()));
        let event = loop {
            match events.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(e @ Event::SourceMissing { .. })) => break e,
                Ok(Some(_)) => continue,
                other => panic!("no SourceMissing: {other:?}"),
            }
        };
        assert_eq!(event, Event::SourceMissing { source: "device:A:0,0,32,32".into(), error: missing });
    }
}
