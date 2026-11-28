//! Every file in `assets/brand/`, made from [`Cut::SIMEE`] and [`Style::MARK`]: SVGs, PNGs, the
//! macOS app icon (`.icns`), the menu bar template and the favicon. `examples/brand.rs` writes
//! them; a test checks the committed SVGs still match what the code draws.

use super::raster::Canvas;
use super::{Cut, Facet, Finish, Motion, Pose, Solid, Style, render};

/// A PNG compressed hard: assets are written once and downloaded many times.
fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    enc.set_filter(png::Filter::Adaptive);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(rgba).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// A file to write under `assets/brand/`.
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    /// Relative to `assets/brand/`.
    pub path: String,
    pub bytes: Vec<u8>,
}

/// The app icon's ground: a squircle (a superellipse of exponent 5) the size of Apple's icon grid
/// body, 824 of 1024 points, lit from the upper left like the stone.
pub struct Ground {
    /// Radial stops from the light outward: offset 0–1 and sRGB.
    pub stops: [(f32, [u8; 3]); 3],
}

impl Ground {
    pub const DARK: Ground = Ground { stops: [(0.0, [0x0f, 0x5a, 0x3c]), (0.55, [0x06, 0x32, 0x1f]), (1.0, [0x02, 0x1a, 0x10])] };
    pub const LIGHT: Ground = Ground { stops: [(0.0, [0xff, 0xff, 0xff]), (0.6, [0xe4, 0xf3, 0xea]), (1.0, [0xc9, 0xe6, 0xd5])] };

    fn at(&self, t: f32) -> [u8; 3] {
        let t = t.clamp(0.0, 1.0);
        let s = &self.stops;
        let (a, b) = if t <= s[1].0 { (s[0], s[1]) } else { (s[1], s[2]) };
        let k = ((t - a.0) / (b.0 - a.0)).clamp(0.0, 1.0);
        std::array::from_fn(|i| (f32::from(a.1[i]) + (f32::from(b.1[i]) - f32::from(a.1[i])) * k).round() as u8)
    }
}

/// The icon's layout in a 100-unit box: body half-width, the stone's centre and scale.
const BODY: f32 = 40.23;
const STONE_AT: [f32; 2] = [50.0, 51.0];
const STONE_SCALE: f32 = 35.0;
/// Where the ground's light sits in the body's box, and the gradient's radius.
const GLOW: [f32; 3] = [0.38, 0.28, 0.85];

fn squircle(points: usize) -> Vec<[f32; 2]> {
    (0..points)
        .map(|i| {
            let t = i as f32 / points as f32 * std::f32::consts::TAU;
            let (s, c) = t.sin_cos();
            let e = 2.0 / 5.0;
            [50.0 + BODY * c.signum() * c.abs().powf(e), 50.0 + BODY * s.signum() * s.abs().powf(e)]
        })
        .collect()
}

fn mark(style: &Style) -> Vec<Facet> {
    let solid = Solid::new(&Cut::SIMEE);
    render(&solid, &Pose::rest(style), style)
}

fn rgb_hex(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn num(x: f32) -> String {
    let s = format!("{x:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

fn polygons(facets: &[Facet], seam: f32) -> String {
    let mut out = String::new();
    for f in facets {
        let pts: Vec<String> = f.points.iter().map(|p| format!("{},{}", num(p[0]), num(p[1]))).collect();
        let fill = rgb_hex(f.rgba);
        if f.rgba[3] == 255 {
            // An outline in the facet's own colour closes the hairline browsers leave between
            // two antialiased fills.
            out += &format!(
                "<polygon points=\"{}\" fill=\"{fill}\" stroke=\"{fill}\" stroke-width=\"{}\" stroke-linejoin=\"round\"/>\n",
                pts.join(" "),
                num(seam)
            );
        } else {
            out += &format!(
                "<polygon points=\"{}\" fill=\"{fill}\" fill-opacity=\"{}\"/>\n",
                pts.join(" "),
                num(f32::from(f.rgba[3]) / 255.0)
            );
        }
    }
    out
}

/// The stone alone, cropped to its outline with a small margin: for READMEs and docs.
pub fn mark_svg(style: &Style) -> String {
    let facets = mark(style);
    let [x0, y0, x1, y1] = facets
        .iter()
        .flat_map(|f| f.points.iter())
        .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])]);
    let m = 0.02;
    let (x, y, w, h) = (x0 - m, y0 - m, x1 - x0 + 2.0 * m, y1 - y0 + 2.0 * m);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\" width=\"{}\" height=\"{}\">\n<title>simee</title>\n{}</svg>\n",
        num(x),
        num(y),
        num(w),
        num(h),
        (w * 128.0).round(),
        (h * 128.0).round(),
        polygons(&facets, 0.006)
    )
}

/// The app icon: the stone on its ground, in a 100-unit box.
pub fn icon_svg(ground: &Ground, style: &Style) -> String {
    let path: Vec<String> = squircle(192).iter().map(|p| format!("{} {}", num(p[0]), num(p[1]))).collect();
    let d = format!("M{}Z", path.join("L"));
    let stops: String = ground
        .stops
        .iter()
        .map(|(o, c)| format!("<stop offset=\"{}\" stop-color=\"{}\"/>", num(*o), rgb_hex([c[0], c[1], c[2], 255])))
        .collect();
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\" width=\"1024\" height=\"1024\">\n<title>simee</title>\n<defs>\
<radialGradient id=\"ground\" cx=\"{}\" cy=\"{}\" r=\"{}\">{stops}</radialGradient>\
<linearGradient id=\"sheen\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\"><stop offset=\"0\" stop-color=\"#fff\" stop-opacity=\"0.14\"/><stop offset=\"0.5\" stop-color=\"#fff\" stop-opacity=\"0\"/></linearGradient>\
</defs>\n<path d=\"{d}\" fill=\"url(#ground)\"/>\n<path d=\"{d}\" fill=\"url(#sheen)\"/>\n<g transform=\"translate({} {}) scale({})\">\n{}</g>\n</svg>\n",
        num(GLOW[0]),
        num(GLOW[1]),
        num(GLOW[2]),
        num(STONE_AT[0]),
        num(STONE_AT[1]),
        num(STONE_SCALE),
        polygons(&mark(style), 0.008)
    )
}

/// The stone alone on a transparent square, `size` pixels: what the box of `style` holds.
pub fn mark_png(size: u32, style: &Style) -> Result<Vec<u8>, String> {
    let mut c = Canvas::new(size);
    let s = size as f32 / 2.0;
    for f in mark(style) {
        let pts: Vec<[f32; 2]> = f.points.iter().map(|p| [(p[0] + 1.0) * s, (p[1] + 1.0) * s]).collect();
        c.fill(&pts, f.rgba);
    }
    encode_rgba(size, size, &c.into_rgba())
}

/// The app icon at `size` pixels. Under 48 px the stone is drawn in flat tones.
pub fn icon_png(size: u32, ground: &Ground) -> Result<Vec<u8>, String> {
    encode_rgba(size, size, &icon_rgba(size, ground))
}

/// [`icon_png`]'s pixels: straight-alpha RGBA rows.
pub fn icon_rgba(size: u32, ground: &Ground) -> Vec<u8> {
    let style = if size < 48 { Style::MARK.finish(Finish::Flat) } else { Style::MARK };
    let k = size as f32 / 100.0;
    let mut c = Canvas::new(size);
    let body: Vec<[f32; 2]> = squircle(256).iter().map(|p| [p[0] * k, p[1] * k]).collect();
    let (x0, w) = ((50.0 - BODY) * k, 2.0 * BODY * k);
    let centre = [x0 + GLOW[0] * w, x0 + GLOW[1] * w];
    c.fill_with(&body, |x, y| {
        let rgb = ground.at((x - centre[0]).hypot(y - centre[1]) / (GLOW[2] * w));
        // The sheen: white at 14% at the top, gone by the middle.
        let sheen = (0.14 * (1.0 - (y - x0) / (0.5 * w))).clamp(0.0, 0.14);
        let lit: [u8; 3] = std::array::from_fn(|i| (f32::from(rgb[i]) + (255.0 - f32::from(rgb[i])) * sheen).round() as u8);
        [lit[0], lit[1], lit[2], 255]
    });
    for f in mark(&style) {
        let pts: Vec<[f32; 2]> =
            f.points.iter().map(|p| [(STONE_AT[0] + p[0] * STONE_SCALE) * k, (STONE_AT[1] + p[1] * STONE_SCALE) * k]).collect();
        c.fill(&pts, f.rgba);
    }
    c.into_rgba()
}

/// Every motion state, one row each in [`Motion::ALL`]'s order, `frames` moments across one
/// period, `cell` pixels square on transparent ground. Progress fills from empty to full along
/// its row.
pub fn motion_sheet(cell: u32, frames: u32) -> Result<Vec<u8>, String> {
    let solid = Solid::new(&Cut::SIMEE);
    let style = Style::MARK;
    let (w, h) = (cell * frames, cell * Motion::ALL.len() as u32);
    // One canvas per cell (the rasterizer is square), copied into the sheet.
    let mut sheet = vec![0u8; (w * h * 4) as usize];
    for (row, motion) in Motion::ALL.iter().enumerate() {
        for k in 0..frames {
            let m = match motion {
                Motion::Progress(_) => Motion::Progress(k as f32 / (frames - 1).max(1) as f32),
                m => *m,
            };
            let pose = m.pose(motion.period() * k as f32 / frames as f32, solid.sides, &style);
            let mut c = Canvas::new(cell);
            let s = cell as f32 / 2.0;
            for f in render(&solid, &pose, &style) {
                let pts: Vec<[f32; 2]> = f.points.iter().map(|p| [(p[0] + 1.0) * s, (p[1] + 1.0) * s]).collect();
                c.fill(&pts, f.rgba);
            }
            let px = c.into_rgba();
            for y in 0..cell {
                let src = (y * cell * 4) as usize;
                let dst = (((row as u32 * cell + y) * w + k * cell) * 4) as usize;
                sheet[dst..dst + (cell * 4) as usize].copy_from_slice(&px[src..src + (cell * 4) as usize]);
            }
        }
    }
    encode_rgba(w, h, &sheet)
}

/// An Apple icon family from square RGBA images, each under the OSType that names its size.
///
/// 16 and 32 px go in `ic04` and `ic05` as ARGB, each channel run-length coded, as `iconutil`
/// writes them: macOS reads PNG data in the older 16 and 32 px slots (`icp4`, `icp5`) as raw
/// pixels and shows noise. Everything else is PNG. Retina entries repeat a size under a second
/// type: ic11 is 16@2x, ic12 32@2x, ic13 128@2x, ic14 256@2x. There is no 64-point icon
/// (iconutil reads icp6 as 48 px).
pub fn icns(images: &[(u32, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let types: &[(u32, &[u8; 4])] = &[
        (16, b"ic04"),
        (32, b"ic05"),
        (32, b"ic11"),
        (64, b"ic12"),
        (128, b"ic07"),
        (256, b"ic08"),
        (256, b"ic13"),
        (512, b"ic09"),
        (512, b"ic14"),
        (1024, b"ic10"),
    ];
    let mut body = Vec::new();
    for (size, ty) in types {
        let Some((_, rgba)) = images.iter().find(|(s, _)| s == size) else { continue };
        let data = if matches!(*ty, b"ic04" | b"ic05") { argb(rgba) } else { encode_rgba(*size, *size, rgba)? };
        body.extend_from_slice(*ty);
        body.extend_from_slice(&(data.len() as u32 + 8).to_be_bytes());
        body.extend_from_slice(&data);
    }
    let mut out = b"icns".to_vec();
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// An `ic04`/`ic05` payload: `ARGB`, then the alpha, red, green and blue planes one after
/// another, each run-length coded.
fn argb(rgba: &[u8]) -> Vec<u8> {
    let mut out = b"ARGB".to_vec();
    for channel in [3, 0, 1, 2] {
        let plane: Vec<u8> = rgba.iter().skip(channel).step_by(4).copied().collect();
        out.extend_from_slice(&run_length(&plane));
    }
    out
}

/// The icon family's run-length code: a byte under 0x80 is followed by that many plus one
/// literal bytes; a byte from 0x80 repeats the next byte that minus 0x7d times (3 to 130).
fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64 + 1);
    let mut literal: Vec<u8> = Vec::new();
    let flush = |literal: &mut Vec<u8>, out: &mut Vec<u8>| {
        for piece in literal.chunks(128) {
            out.push((piece.len() - 1) as u8);
            out.extend_from_slice(piece);
        }
        literal.clear();
    };
    let mut i = 0;
    while i < data.len() {
        let run = data[i..].iter().take(130).take_while(|&&b| b == data[i]).count();
        if run >= 3 {
            flush(&mut literal, &mut out);
            out.push((run - 3 + 0x80) as u8);
            out.push(data[i]);
            i += run;
        } else {
            literal.push(data[i]);
            i += 1;
        }
    }
    flush(&mut literal, &mut out);
    out
}

/// A Windows icon file holding PNGs (Vista and later, and every browser).
pub fn ico(pngs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(pngs.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * pngs.len() as u32;
    for (size, png) in pngs {
        let side = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in pngs {
        out.extend_from_slice(png);
    }
    out
}

/// The menu bar template: black facets at three opacities, nearly filling its square.
pub const TEMPLATE: Style = Style::MARK.finish(Finish::Template).pad(0.96).tilt(0.12);
/// Favicons: four flat tones, nearly filling the square.
pub const FAVICON: Style = Style::MARK.finish(Finish::Flat).pad(0.96);
/// The mark's PNGs: shaded, nearly filling the square.
pub const MARK_PNG: Style = Style::MARK.pad(0.94);

/// The sizes every square PNG comes in.
pub const SIZES: [u32; 7] = [16, 32, 64, 128, 256, 512, 1024];

fn asset(path: &str, bytes: Vec<u8>) -> Asset {
    Asset { path: path.into(), bytes }
}

/// The SVGs in `assets/brand/`.
pub fn svgs() -> Vec<Asset> {
    vec![
        asset("simee-mark.svg", mark_svg(&Style::MARK).into_bytes()),
        asset("simee-mark-flat.svg", mark_svg(&Style::MARK.finish(Finish::Flat)).into_bytes()),
        asset("simee-template.svg", mark_svg(&TEMPLATE).into_bytes()),
        asset("simee-icon.svg", icon_svg(&Ground::DARK, &Style::MARK).into_bytes()),
        asset("simee-icon-light.svg", icon_svg(&Ground::LIGHT, &Style::MARK).into_bytes()),
    ]
}

/// The rasters in `assets/brand/`: PNGs, the `.icns`, the menu bar template and the favicon.
pub fn rasters() -> Result<Vec<Asset>, String> {
    let mut out = Vec::new();
    let mut icons = Vec::new();
    for size in SIZES {
        let rgba = icon_rgba(size, &Ground::DARK);
        out.push(asset(&format!("png/simee-icon-{size}.png"), encode_rgba(size, size, &rgba)?));
        out.push(asset(&format!("png/simee-icon-light-{size}.png"), icon_png(size, &Ground::LIGHT)?));
        let style = if size < 48 { FAVICON } else { MARK_PNG };
        out.push(asset(&format!("png/simee-mark-{size}.png"), mark_png(size, &style)?));
        icons.push((size, rgba));
    }
    out.push(asset("macos/simee.icns", icns(&icons)?));
    out.push(asset("macos/simeeTemplate.png", mark_png(16, &TEMPLATE)?));
    out.push(asset("macos/simeeTemplate@2x.png", mark_png(32, &TEMPLATE)?));
    let favicons = [16, 32, 48].into_iter().map(|s| Ok((s, mark_png(s, &FAVICON)?))).collect::<Result<Vec<_>, String>>()?;
    out.push(asset("favicon.ico", ico(&favicons)));
    out.push(asset("png/simee-motion.png", motion_sheet(64, 8)?));
    Ok(out)
}

/// Every file in `assets/brand/`.
pub fn all() -> Result<Vec<Asset>, String> {
    let mut out = svgs();
    out.extend(rasters()?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::png_size;
    use std::path::Path;

    fn brand_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/brand")
    }

    /// Text with every number replaced by a placeholder, and the numbers: compares SVGs drawn by
    /// different hosts' maths libraries, whose last digits may differ.
    fn split_numbers(s: &str) -> (String, Vec<f64>) {
        let (mut text, mut nums, mut cur) = (String::new(), Vec::new(), String::new());
        for ch in s.chars().chain(std::iter::once(' ')) {
            if ch.is_ascii_digit() || ch == '.' || (ch == '-' && cur.is_empty()) {
                cur.push(ch);
                continue;
            }
            if let Ok(v) = cur.parse::<f64>() {
                nums.push(v);
                text.push('#');
            } else {
                text += &cur;
            }
            cur.clear();
            text.push(ch);
        }
        (text, nums)
    }

    /// `scripts/brand.sh` regenerates them; this fails when the code drawing the mark changed
    /// and the committed assets didn't.
    #[test]
    fn committed_svgs_match_the_code() {
        for a in svgs() {
            let path = brand_dir().join(&a.path);
            let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e} (run scripts/brand.sh)", path.display()));
            let (t1, n1) = split_numbers(&committed);
            let (t2, n2) = split_numbers(std::str::from_utf8(&a.bytes).expect("utf-8"));
            assert_eq!(t1, t2, "{} is stale: run scripts/brand.sh", a.path);
            assert_eq!(n1.len(), n2.len(), "{}", a.path);
            assert!(n1.iter().zip(&n2).all(|(a, b)| (a - b).abs() <= 0.0015), "{} is stale: run scripts/brand.sh", a.path);
        }
    }

    /// The rasters are drawn from the same facets as the SVGs above; drawing them all here would
    /// take seconds in a debug build, so this only checks each is committed at its size.
    #[test]
    fn every_raster_is_committed_at_its_size() {
        let dir = brand_dir();
        let size_of = |p: &str| png_size(&std::fs::read(dir.join(p)).unwrap_or_else(|e| panic!("{p}: {e} (run scripts/brand.sh)")));
        for s in SIZES {
            for name in ["simee-icon", "simee-icon-light", "simee-mark"] {
                assert_eq!(size_of(&format!("png/{name}-{s}.png")), Some((s, s)), "{name}-{s}");
            }
        }
        assert_eq!(size_of("png/simee-motion.png"), Some((64 * 8, 64 * Motion::ALL.len() as u32)));
        assert_eq!(size_of("macos/simeeTemplate.png"), Some((16, 16)));
        assert_eq!(size_of("macos/simeeTemplate@2x.png"), Some((32, 32)));
        let icns = std::fs::read(dir.join("macos/simee.icns")).expect("simee.icns");
        assert_eq!(&icns[..4], b"icns");
        assert!(icns.windows(4).any(|w| w == b"ic04") && !icns.windows(4).any(|w| w == b"icp4"), "16 px is ARGB, not PNG");
        assert_eq!(&std::fs::read(dir.join("favicon.ico")).expect("favicon.ico")[..4], &[0, 0, 1, 0]);
    }

    #[test]
    fn the_icns_holds_every_size() {
        let images: Vec<_> = SIZES.map(|s| (s, vec![s as u8; (s * s * 4) as usize])).into();
        let icns = icns(&images).expect("icns");
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(u32::from_be_bytes(icns[4..8].try_into().expect("length")) as usize, icns.len());
        for ty in [b"ic04", b"ic05", b"ic11", b"ic12", b"ic07", b"ic08", b"ic13", b"ic09", b"ic14", b"ic10"] {
            assert!(icns.windows(4).any(|w| w == ty), "{}", String::from_utf8_lossy(ty));
        }
        assert!(!icns.windows(4).any(|w| w == b"icp4" || w == b"icp5"), "no PNG in the 16 and 32 px slots");
    }

    /// The inverse of [`run_length`], `n` bytes of it, and how many coded bytes it took.
    fn run_length_decode(data: &[u8], n: usize) -> (Vec<u8>, usize) {
        let (mut out, mut i) = (Vec::with_capacity(n), 0);
        while out.len() < n {
            let b = usize::from(data[i]);
            if b < 0x80 {
                out.extend_from_slice(&data[i + 1..i + 2 + b]);
                i += 2 + b;
            } else {
                out.extend(std::iter::repeat_n(data[i + 1], b - 0x7d));
                i += 2;
            }
        }
        (out, i)
    }

    #[test]
    fn run_length_round_trips_runs_literals_and_long_stretches() {
        let mut data = vec![7u8; 300];
        data.extend((0..=255).map(|b: u8| b));
        data.extend([1, 1, 2, 2, 2, 3]);
        let coded = run_length(&data);
        assert!(coded.len() < data.len(), "runs shrink");
        assert_eq!(run_length_decode(&coded, data.len()), (data, coded.len()));
    }

    #[test]
    fn argb_holds_the_four_planes_in_order() {
        let rgba = [10u8, 20, 30, 40].repeat(16);
        let payload = argb(&rgba);
        assert_eq!(&payload[..4], b"ARGB");
        let (planes, used) = run_length_decode(&payload[4..], 64);
        assert_eq!(used, payload.len() - 4);
        assert_eq!(planes, [[40u8; 16], [10; 16], [20; 16], [30; 16]].concat());
    }

    fn decode_rgba(png: &[u8]) -> (u32, Vec<u8>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().expect("header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            other => panic!("unexpected PNG colour type {other:?}"),
        };
        (info.width, rgba)
    }

    /// macOS's own reader gives back the pixels written, at every size: no noise in the 16 and
    /// 32 px slots. Hosts without `iconutil` (Linux) skip it.
    #[test]
    fn iconutil_reads_back_every_size() {
        let images: Vec<_> = [16, 32, 64, 128].into_iter().map(|s| (s, icon_rgba(s, &Ground::DARK))).collect();
        let dir = std::env::temp_dir().join(format!("simee-icns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let (file, set) = (dir.join("t.icns"), dir.join("t.iconset"));
        std::fs::write(&file, icns(&images).expect("icns")).expect("write");
        let status = std::process::Command::new("iconutil").arg("-c").arg("iconset").arg(&file).arg("-o").arg(&set).status();
        let Ok(status) = status else {
            // No iconutil (not macOS, or no command line tools): nothing to compare with.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        assert!(status.success(), "iconutil refused the file");
        for (name, size) in [("icon_16x16", 16), ("icon_32x32", 32), ("icon_16x16@2x", 32), ("icon_32x32@2x", 64), ("icon_128x128", 128)] {
            let (width, got) = decode_rgba(&std::fs::read(set.join(format!("{name}.png"))).expect(name));
            let want = &images.iter().find(|(s, _)| *s == size).expect("size").1;
            assert_eq!(width, size, "{name}");
            let worst = got
                .as_chunks::<4>()
                .0
                .iter()
                .zip(want.as_chunks::<4>().0)
                .filter(|(_, w)| w[3] == 255)
                .map(|(g, w)| (0..4).map(|k| g[k].abs_diff(w[k])).max().unwrap_or(0))
                .max()
                .unwrap_or(0);
            assert!(worst <= 2, "{name}: an opaque pixel is off by {worst}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ico_points_at_each_png() {
        let pngs = vec![(16, vec![1u8; 5]), (32, vec![2u8; 7])];
        let ico = ico(&pngs);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 2, 0]);
        let entry = |i: usize| {
            let e = &ico[6 + 16 * i..22 + 16 * i];
            (
                e[0],
                u32::from_le_bytes(e[8..12].try_into().expect("size")),
                u32::from_le_bytes(e[12..16].try_into().expect("offset")) as usize,
            )
        };
        let (w, len, at) = entry(1);
        assert_eq!((w, len), (32, 7));
        assert_eq!(&ico[at..at + 7], &[2u8; 7]);
    }

    #[test]
    fn the_icon_is_opaque_inside_and_clear_at_its_corners() {
        let png = icon_png(64, &Ground::DARK).expect("icon");
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().expect("header");
        let mut px = vec![0; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut px).expect("frame");
        let alpha = |x: usize, y: usize| px[(y * 64 + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(32, 32), 255);
    }

    #[test]
    fn the_mark_svg_is_taller_than_wide() {
        let svg = mark_svg(&Style::MARK);
        let (_, nums) = split_numbers(svg.split("viewBox=\"").nth(1).and_then(|v| v.split('"').next()).expect("viewBox"));
        assert!(nums[3] / nums[2] > 1.3, "{nums:?}");
    }
}
