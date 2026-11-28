//! Synthetic sources: no hardware, the same frames on every host. The test card is drawn fresh
//! each tick; the others are stills, drawn once per size and shared (no copy) after that.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use qrcodegen::{QrCode, QrCodeEcc};

use crate::draw::{Canvas, Colour, GLYPH};
use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick, fill};
use crate::id::{Pattern, SourceSpec};
use crate::source::Source;

/// Opens a synthetic source.
pub fn open(source: &SourceSpec) -> Result<Box<dyn Source>> {
    Ok(match source {
        SourceSpec::Pattern(Pattern::Testcard) => Box::new(Testcard),
        SourceSpec::Qr(text) => {
            let qr = QrCode::encode_text(text, QrCodeEcc::Medium)
                .map_err(|e| Error::Invalid(format!("qr: {} bytes is too long for a QR code: {e}", text.len())))?;
            Box::new(Still::new(move |spec| draw_qr(&qr, spec.width, spec.height)))
        }
        SourceSpec::Text(text) => {
            let text = text.clone();
            Box::new(Still::new(move |spec| {
                let mut c = Canvas::new(spec.width, spec.height, Colour::rgb(16, 16, 16));
                c.centred_text(&text, Colour::WHITE, 16);
                Ok(c.bgra)
            }))
        }
        SourceSpec::Image(path) => {
            let (w, h, bgra) = load_image(path)?;
            Box::new(Still::new(move |spec| Ok(fill(w, h, &bgra, spec.width, spec.height))))
        }
        SourceSpec::Solid(colour) => {
            let colour = Colour::parse(colour)?;
            Box::new(Still::new(move |spec| Ok(Canvas::new(spec.width, spec.height, colour).bgra)))
        }
        SourceSpec::Video(path) => return open_video(path),
        SourceSpec::Webcam(_) => return Err(Error::Invalid("a webcam is not a synthetic source".into())),
        SourceSpec::Device(_) => return Err(Error::Invalid("a device's screen is not a synthetic source".into())),
        SourceSpec::Feed(_) => return Err(Error::Invalid("a camera feed is not a synthetic source".into())),
    })
}

/// A video file, looped at its own frame rate: decoded with AVFoundation on macOS.
#[cfg(target_os = "macos")]
fn open_video(path: &Path) -> Result<Box<dyn Source>> {
    crate::avf::open_video(path)
}

#[cfg(not(target_os = "macos"))]
fn open_video(path: &Path) -> Result<Box<dyn Source>> {
    Err(Error::Unsupported(format!(
        "video:{}: video files are decoded with AVFoundation, so they play on macOS only; here, use image: for a still, or play the file into a v4l2loopback camera and use webcam:",
        path.display()
    )))
}

type Render = dyn Fn(FrameSpec) -> Result<Vec<u8>> + Send;

/// A source whose frames don't change: drawn once per size, then shared.
struct Still {
    render: Box<Render>,
    cached: HashMap<(u32, u32), Arc<[u8]>>,
}

/// Sizes a still keeps drawn at once; more distinct sizes than this start over.
const STILL_SIZES: usize = 8;

impl Still {
    fn new(render: impl Fn(FrameSpec) -> Result<Vec<u8>> + Send + 'static) -> Still {
        Still { render: Box::new(render), cached: HashMap::new() }
    }
}

impl Source for Still {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        let size = (spec.width, spec.height);
        let bgra = match self.cached.get(&size) {
            Some(bgra) => bgra.clone(),
            None => {
                let bgra: Arc<[u8]> = (self.render)(spec)?.into();
                if self.cached.len() >= STILL_SIZES {
                    self.cached.clear();
                }
                self.cached.insert(size, bgra.clone());
                bgra
            }
        };
        Ok(CameraFrame { width: spec.width, height: spec.height, bgra, tick })
    }

    fn synthetic(&self) -> bool {
        true
    }
}

/// Modules of white around a QR code: the spec's minimum quiet zone.
pub const QR_QUIET: i32 = 4;

/// Black modules on white, as large as fit, centred.
fn draw_qr(qr: &QrCode, width: u32, height: u32) -> Result<Vec<u8>> {
    let (module, x0, y0) = qr_layout(qr.size(), width, height)?;
    let mut c = Canvas::new(width, height, Colour::WHITE);
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                c.rect(x0 + x as u32 * module, y0 + y as u32 * module, module, module, Colour::BLACK);
            }
        }
    }
    Ok(c.bgra)
}

/// The module size and the top-left corner of the code's first module in a `width`x`height`
/// frame.
pub fn qr_layout(size: i32, width: u32, height: u32) -> Result<(u32, u32, u32)> {
    let total = (size + 2 * QR_QUIET) as u32;
    let module = width.min(height) / total;
    if module == 0 {
        return Err(Error::Invalid(format!("a {width}x{height} frame is too small for a {size}-module QR code")));
    }
    let x0 = (width - size as u32 * module) / 2;
    let y0 = (height - size as u32 * module) / 2;
    Ok((module, x0, y0))
}

/// Decodes a PNG or JPEG file to BGRA, composited over black.
fn load_image(path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    let bytes = std::fs::read(path).map_err(|e| Error::Io(format!("image:{}: {e}", path.display())))?;
    let what = |e: String| Error::Invalid(format!("image:{}: {e}", path.display()));
    if bytes.starts_with(b"\x89PNG") {
        decode_png(&bytes).map_err(what)
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        decode_jpeg(&bytes).map_err(what)
    } else {
        Err(what("not a PNG or JPEG file".into()))
    }
}

/// The longest side an `image:` file (or a `video:` file's frame) may have, and the most pixels:
/// bigger files are refused before anything is allocated for them (a header can claim far more
/// than its bytes hold).
pub const MAX_IMAGE_SIDE: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 36_000_000;

fn check_size(width: u32, height: u32) -> std::result::Result<(), String> {
    check_size_of("image", width, height)
}

/// Checks a `kind:` source's picture size against [`MAX_IMAGE_SIDE`] and [`MAX_IMAGE_PIXELS`].
pub(crate) fn check_size_of(kind: &str, width: u32, height: u32) -> std::result::Result<(), String> {
    if width == 0
        || height == 0
        || width > MAX_IMAGE_SIDE
        || height > MAX_IMAGE_SIDE
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(format!(
            "{width}x{height} is outside what {kind}: accepts (up to {MAX_IMAGE_SIDE} px a side and {} megapixels)",
            MAX_IMAGE_PIXELS / 1_000_000
        ));
    }
    Ok(())
}

/// The size a PNG's IHDR claims.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let ihdr = bytes.get(16..24)?;
    let word = |i: usize| u32::from_be_bytes([ihdr[i], ihdr[i + 1], ihdr[i + 2], ihdr[i + 3]]);
    (bytes.get(12..16)? == b"IHDR").then(|| (word(0), word(4)))
}

/// The size a JPEG's first start-of-frame segment claims.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        if marker == 0xff {
            i += 1;
            continue;
        }
        if marker == 0x00 || marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            i += 2;
            continue;
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let sof = bytes.get(i + 5..i + 9)?;
            return Some((u32::from(u16::from_be_bytes([sof[2], sof[3]])), u32::from(u16::from_be_bytes([sof[0], sof[1]]))));
        }
        i += 2 + usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
    }
    None
}

fn decode_png(bytes: &[u8]) -> std::result::Result<(u32, u32, Vec<u8>), String> {
    let (w, h) = png_size(bytes).ok_or("not a valid PNG header")?;
    check_size(w, h)?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    check_size(reader.info().width, reader.info().height)?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    let over_black = |r: u8, g: u8, b: u8, a: u8| {
        let m = |c: u8| (u16::from(c) * u16::from(a) / 255) as u8;
        [m(b), m(g), m(r), 0xff]
    };
    let bgra: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf.as_chunks::<4>().0.iter().flat_map(|p| over_black(p[0], p[1], p[2], p[3])).collect(),
        png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[2], p[1], p[0], 0xff]).collect(),
        png::ColorType::GrayscaleAlpha => buf.as_chunks::<2>().0.iter().flat_map(|p| over_black(p[0], p[0], p[0], p[1])).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 0xff]).collect(),
        png::ColorType::Indexed => return Err("indexed PNG wasn't expanded".into()),
    };
    Ok((info.width, info.height, bgra))
}

fn decode_jpeg(bytes: &[u8]) -> std::result::Result<(u32, u32, Vec<u8>), String> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let (w, h) = jpeg_size(bytes).ok_or("no frame header in the JPEG")?;
    check_size(w, h)?;
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(MAX_IMAGE_SIDE as usize)
        .set_max_height(MAX_IMAGE_SIDE as usize);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let rgb = decoder.decode().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("JPEG has no header")?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if rgb.len() != w as usize * h as usize * 3 {
        return Err(format!("JPEG decoded to {} bytes for {w}x{h} RGB", rgb.len()));
    }
    Ok((w, h, rgb.as_chunks::<3>().0.iter().flat_map(|p| [p[2], p[1], p[0], 0xff]).collect()))
}

/// The test card's bars, left to right (75% bars, as SMPTE's).
pub const BARS: [Colour; 7] = [
    Colour::rgb(191, 191, 191),
    Colour::rgb(191, 191, 0),
    Colour::rgb(0, 191, 191),
    Colour::rgb(0, 191, 0),
    Colour::rgb(191, 0, 191),
    Colour::rgb(191, 0, 0),
    Colour::rgb(0, 0, 191),
];

/// Bits in the test card's frame-number strip.
pub const STRIP_BITS: u32 = 32;

/// The test card: a strip across the top carrying the frame number as 32 black/white cells (most
/// significant first, machine-readable after scaling), colour bars, a marker that moves each
/// frame, and the frame number, size and wall-clock time (UTC) as text, so a consumer can see
/// both pacing and end-to-end latency.
struct Testcard;

impl Source for Testcard {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        let (w, h) = (spec.width, spec.height);
        let mut c = Canvas::new(w, h, Colour::BLACK);
        let strip = strip_height(h);
        let cell = w / STRIP_BITS;
        let seq = tick.seq as u32;
        for bit in 0..STRIP_BITS {
            if seq >> (STRIP_BITS - 1 - bit) & 1 == 1 {
                c.rect(bit * cell, 0, cell, strip, Colour::WHITE);
            }
        }
        let bars_top = strip + strip / 2;
        let bars_end = h * 5 / 8;
        let bar_w = w / BARS.len() as u32;
        for (i, colour) in BARS.iter().enumerate() {
            let x = i as u32 * bar_w;
            let width = if i == BARS.len() - 1 { w - x } else { bar_w };
            c.rect(x, bars_top, width, bars_end.saturating_sub(bars_top), *colour);
        }
        let band = (h / 16).max(2);
        c.rect(0, bars_end, w, band, Colour::rgb(48, 48, 48));
        let marker_w = (w / 40).max(2);
        let step = (w / 60).max(1);
        let x = (tick.seq % u64::from((w / step).max(1))) as u32 * step;
        c.rect(x, bars_end, marker_w, band, Colour::WHITE);

        let lines = [
            "SIMEE TESTCARD".to_string(),
            format!("#{} {}x{}@{}", tick.seq, w, h, spec.fps),
            format!("{} UTC +{:.3}s", clock(tick.wall), tick.pts.as_secs_f64()),
        ];
        let text_top = bars_end + band + band / 2;
        let longest = lines.iter().map(|l| l.len() as u32).max().unwrap_or(1);
        let avail_h = h.saturating_sub(text_top);
        let scale = ((w * 9 / 10) / (longest * GLYPH)).min(avail_h / (lines.len() as u32 * (GLYPH + 2))).max(1);
        for (i, line) in lines.iter().enumerate() {
            let y = text_top + i as u32 * (GLYPH + 2) * scale;
            let lw = line.len() as u32 * GLYPH * scale;
            c.text(w.saturating_sub(lw) / 2, y, scale, line, Colour::WHITE);
        }
        Ok(CameraFrame { width: w, height: h, bgra: c.bgra.into(), tick })
    }

    fn synthetic(&self) -> bool {
        true
    }
}

fn strip_height(h: u32) -> u32 {
    (h / 16).max(2)
}

/// `HH:MM:SS.mmm` of a wall-clock time, UTC.
fn clock(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let day = since.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}", day / 3600, day / 60 % 60, day % 60, since.subsec_millis())
}

/// Reads the frame number back from a test card frame's top strip (the low 32 bits of
/// [`Tick::seq`]), also after the frame was scaled; `None` when the frame is too small to carry it.
pub fn testcard_seq(frame: &CameraFrame) -> Option<u32> {
    let cell = frame.width / STRIP_BITS;
    if cell == 0 || frame.height < 2 {
        return None;
    }
    let y = strip_height(frame.height) / 2;
    let mut seq = 0u32;
    for bit in 0..STRIP_BITS {
        let [b, g, r, _] = frame.pixel(bit * cell + cell / 2, y);
        let lit = (u32::from(r) + u32::from(g) + u32::from(b)) / 3 > 127;
        seq = seq << 1 | u32::from(lit);
    }
    Some(seq)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn spec(w: u32, h: u32) -> FrameSpec {
        FrameSpec::new(w, h, 30).unwrap_or_default()
    }

    fn frame_of(id: &str, spec: FrameSpec, tick: Tick) -> CameraFrame {
        let source = SourceSpec::parse(id).unwrap_or_else(|e| panic!("{id}: {e}"));
        let mut s = open(&source).unwrap_or_else(|e| panic!("{id}: {e}"));
        s.frame(spec, tick).unwrap_or_else(|e| panic!("{id}: {e}"))
    }

    fn assert_frame_shape(f: &CameraFrame, spec: FrameSpec) {
        assert_eq!((f.width, f.height), (spec.width, spec.height));
        assert_eq!(f.bgra.len(), spec.width as usize * spec.height as usize * 4);
        assert!(f.bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 0xff), "opaque");
    }

    #[test]
    fn solid_fills_every_pixel() {
        let s = spec(64, 48);
        let f = frame_of("solid:#336699", s, Tick::start());
        assert_frame_shape(&f, s);
        assert!(f.bgra.as_chunks::<4>().0.iter().all(|p| *p == [0x99, 0x66, 0x33, 0xff]));
        let named = frame_of("solid:orange", s, Tick::start());
        assert_eq!(named.pixel(10, 10), [0, 165, 255, 255]);
    }

    #[test]
    fn testcard_carries_the_frame_number_and_moves() {
        let s = spec(640, 480);
        let mut card = open(&SourceSpec::Pattern(Pattern::Testcard)).unwrap_or_else(|e| panic!("{e}"));
        let start = Tick::start();
        let mut frames = Vec::new();
        for seq in [0u64, 1, 2, 0xdead_beef, 12345] {
            let tick = Tick { seq, pts: Duration::from_millis(seq % 1000 * 33), wall: start.wall };
            let f = card.frame(s, tick).unwrap_or_else(|e| panic!("{e}"));
            assert_frame_shape(&f, s);
            assert_eq!(testcard_seq(&f), Some(seq as u32), "seq {seq}");
            frames.push(f);
        }
        assert_ne!(frames[0].bgra, frames[1].bgra, "each frame differs");
        // The bars: sample the middle of each one below the strip.
        let y = 480 / 3;
        for (i, colour) in BARS.iter().enumerate() {
            let x = (640 / 7) * i as u32 + 640 / 14;
            assert_eq!(frames[0].pixel(x, y), colour.bgra(), "bar {i}");
        }
        // Scaled down, the strip still reads.
        let small = frames[3].fit(320, 240);
        assert_eq!(testcard_seq(&small), Some(0xdead_beef));
        // The text band has lit pixels (the counter and clock).
        let text_rows = &frames[4].bgra[(480 * 3 / 4 * 640 * 4) as usize..];
        assert!(text_rows.as_chunks::<4>().0.contains(&[255, 255, 255, 255]));
        assert_eq!(clock(UNIX_EPOCH + Duration::from_millis(3_723_004)), "01:02:03.004");
    }

    #[test]
    fn testcard_draws_at_the_minimum_size() {
        let s = spec(32, 32);
        let f = frame_of("pattern:testcard", s, Tick { seq: 7, ..Tick::start() });
        assert_frame_shape(&f, s);
        assert_eq!(testcard_seq(&f), Some(7));
    }

    #[test]
    fn qr_modules_match_the_code() {
        let text = "simee-probe-42";
        let s = spec(400, 300);
        let f = frame_of(&format!("qr:{text}"), s, Tick::start());
        assert_frame_shape(&f, s);
        let qr = QrCode::encode_text(text, QrCodeEcc::Medium).unwrap_or_else(|e| panic!("{e}"));
        let (module, x0, y0) = qr_layout(qr.size(), 400, 300).unwrap_or_default();
        assert!(module >= 2, "module {module}");
        let black = [0, 0, 0, 255];
        let white = [255, 255, 255, 255];
        // Every module's centre (and the quiet zone around the code) has the colour it should.
        for y in -QR_QUIET..qr.size() + QR_QUIET {
            for x in -QR_QUIET..qr.size() + QR_QUIET {
                let px = (x0 as i32 + x * module as i32 + module as i32 / 2) as u32;
                let py = (y0 as i32 + y * module as i32 + module as i32 / 2) as u32;
                let want = if qr.get_module(x, y) { black } else { white };
                assert_eq!(f.pixel(px, py), want, "module {x},{y}");
            }
        }
        // The three finder patterns' dark corners.
        let last = qr.size() - 1;
        assert!(qr.get_module(0, 0) && qr.get_module(last, 0) && qr.get_module(0, last));
        assert_eq!(f.pixel(x0, y0), black);
        // A different text gives a different code.
        assert_ne!(frame_of("qr:other", s, Tick::start()).bgra, f.bgra);
    }

    #[test]
    fn qr_reports_text_that_wont_fit() {
        let err = open(&SourceSpec::Qr("x".repeat(4000))).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("too long for a QR code"), "{err}");
        let small = qr_layout(177, 64, 64).expect_err("must fail").to_string();
        assert!(small.contains("too small"), "{small}");
    }

    #[test]
    fn text_is_drawn_centred_in_white() {
        let s = spec(320, 240);
        let f = frame_of("text:Hello", s, Tick::start());
        assert_frame_shape(&f, s);
        let lit: Vec<(u32, u32)> =
            (0..240).flat_map(|y| (0..320).map(move |x| (x, y))).filter(|&(x, y)| f.pixel(x, y) == [255, 255, 255, 255]).collect();
        assert!(!lit.is_empty());
        let (min_x, max_x) = (lit.iter().map(|p| p.0).min().unwrap_or(0), lit.iter().map(|p| p.0).max().unwrap_or(0));
        let (min_y, max_y) = (lit.iter().map(|p| p.1).min().unwrap_or(0), lit.iter().map(|p| p.1).max().unwrap_or(0));
        let (cx, cy) = ((min_x + max_x) / 2, (min_y + max_y) / 2);
        assert!(cx.abs_diff(160) < 20 && cy.abs_diff(120) < 20, "centre {cx},{cy}");
        assert_eq!(f.pixel(0, 0), [16, 16, 16, 255]);
        assert_ne!(frame_of("text:World", s, Tick::start()).bgra, f.bgra);
    }

    #[test]
    fn stills_are_drawn_once_per_size() {
        let mut s = open(&SourceSpec::Text("hi".into())).unwrap_or_else(|e| panic!("{e}"));
        let a = s.frame(spec(64, 64), Tick::start()).unwrap_or_else(|e| panic!("{e}"));
        let b = s.frame(spec(64, 64), Tick { seq: 1, ..Tick::start() }).unwrap_or_else(|e| panic!("{e}"));
        assert!(Arc::ptr_eq(&a.bgra, &b.bgra));
        assert_eq!(b.tick.seq, 1);
        let c = s.frame(spec(128, 64), Tick::start()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.bgra.len(), 128 * 64 * 4);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sim-camera-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn png_images_are_converted_and_scaled() {
        let dir = temp_dir("png");
        let path = dir.join("quad.png");
        // 2x2 RGBA: red, green / blue, half-transparent white.
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128];
        let png = sim_core::frame::encode_rgba(2, 2, &rgba).unwrap_or_default();
        std::fs::write(&path, png).ok();
        let s = spec(64, 64);
        let f = frame_of(&format!("image:{}", path.display()), s, Tick::start());
        assert_frame_shape(&f, s);
        assert_eq!(f.pixel(0, 0), [0, 0, 255, 255], "red");
        assert_eq!(f.pixel(63, 0), [0, 255, 0, 255], "green");
        assert_eq!(f.pixel(0, 63), [255, 0, 0, 255], "blue");
        assert_eq!(f.pixel(63, 63), [128, 128, 128, 255], "white over black at half alpha");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_images_decode() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/red-16x16.jpg");
        let s = spec(32, 32);
        let f = frame_of(&format!("image:{}", path.display()), s, Tick::start());
        assert_frame_shape(&f, s);
        let [b, g, r, _] = f.pixel(16, 16);
        assert!(r > 230 && g < 30 && b < 30, "red, within JPEG error: {r},{g},{b}");
    }

    #[test]
    fn bad_images_say_what_is_wrong() {
        let dir = temp_dir("bad");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "hello").ok();
        let err = open(&SourceSpec::Image(path)).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("not a PNG or JPEG"), "{err}");
        let missing = open(&SourceSpec::Image(dir.join("missing.png"))).err();
        assert!(matches!(missing, Some(Error::Io(_))), "{missing:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { crc >> 1 ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        let body: Vec<u8> = kind.iter().chain(data).copied().collect();
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
        out
    }

    fn open_bytes(name: &str, bytes: &[u8]) -> String {
        let dir = temp_dir(name);
        let path = dir.join(name);
        std::fs::write(&path, bytes).ok();
        let err = open(&SourceSpec::Image(path)).err().map(|e| e.to_string()).unwrap_or_default();
        std::fs::remove_dir_all(&dir).ok();
        err
    }

    #[test]
    fn a_png_declaring_a_huge_size_is_refused_before_allocating() {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(png_chunk(b"IHDR", &ihdr));
        png.extend(png_chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]));
        png.extend(png_chunk(b"IEND", &[]));
        let err = open_bytes("huge.png", &png);
        assert!(err.contains("60000x60000") && err.contains("image: accepts"), "{err}");
    }

    #[test]
    fn a_jpeg_declaring_a_huge_size_is_refused_before_decoding() {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0, 17, 8];
        jpeg.extend_from_slice(&16_384u16.to_be_bytes());
        jpeg.extend_from_slice(&16_384u16.to_be_bytes());
        jpeg.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xd9]);
        let err = open_bytes("huge.jpg", &jpeg);
        assert!(err.contains("16384x16384") && err.contains("image: accepts"), "{err}");
    }

    #[test]
    fn the_size_limits_pass_ordinary_images() {
        assert!(check_size(8192, 4096).is_ok());
        assert!(check_size(8192, 8192).is_err());
        assert!(check_size(8193, 1).is_err());
        assert_eq!(
            jpeg_size(&std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/red-16x16.jpg")).unwrap_or_default()),
            Some((16, 16))
        );
    }

    /// Video files are decoded with AVFoundation; elsewhere `video:` says so and what to use.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn video_needs_macos() {
        let err = open(&SourceSpec::Video("/tmp/clip.mp4".into())).err();
        assert!(matches!(&err, Some(Error::Unsupported(m)) if m.contains("video:/tmp/clip.mp4") && m.contains("macOS")), "{err:?}");
    }
}
