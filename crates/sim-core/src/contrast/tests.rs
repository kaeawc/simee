use super::*;

/// A deterministic xorshift generator, so noise frames are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

fn rgb(hex: u32) -> Rgb {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8]
}

fn solid(width: u32, height: u32, c: Rgb) -> Frame {
    Frame { width, height, rgba: (0..width * height).flat_map(|_| [c[0], c[1], c[2], 255]).collect() }
}

fn set(f: &mut Frame, x: u32, y: u32, c: Rgb) {
    let i = ((y * f.width + x) * 4) as usize;
    f.rgba[i..i + 3].copy_from_slice(&c);
}

fn get(f: &Frame, x: u32, y: u32) -> Rgb {
    let i = ((y * f.width + x) * 4) as usize;
    [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2]]
}

fn blend(under: Rgb, over: Rgb, coverage: f64) -> Rgb {
    std::array::from_fn(|i| (f64::from(under[i]) + (f64::from(over[i]) - f64::from(under[i])) * coverage).round() as u8)
}

fn fill_rect(f: &mut Frame, x: u32, y: u32, w: u32, h: u32, c: Rgb) {
    for yy in y..(y + h).min(f.height) {
        for xx in x..(x + w).min(f.width) {
            set(f, xx, yy, c);
        }
    }
}

/// A vertical (or horizontal) gradient from `a` to `b` over the whole frame.
fn gradient(width: u32, height: u32, a: Rgb, b: Rgb, vertical: bool) -> Frame {
    let mut f = solid(width, height, a);
    for y in 0..height {
        for x in 0..width {
            let t = if vertical { f64::from(y) / f64::from(height - 1) } else { f64::from(x) / f64::from(width - 1) };
            set(&mut f, x, y, blend(a, b, t));
        }
    }
    f
}

/// A glyph-like 5x7 dot pattern for character `n`: stems and bars, about half the cell inked, and
/// different for each character.
fn glyph(n: u8) -> [[bool; 5]; 7] {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (u64::from(n) + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93));
    let mut g = [[false; 5]; 7];
    for row in &mut g {
        for dot in row.iter_mut() {
            *dot = rng.byte() % 100 < 45;
        }
    }
    // A stem, as most letters have one, and the bars that keep strokes connected.
    let stem = (rng.byte() % 5) as usize;
    for row in &mut g {
        row[stem] = true;
    }
    g[(rng.byte() % 7) as usize] = [true; 5];
    // Every glyph reaches the top and bottom row, so a line's ink height is exactly 7 dots.
    g[0][stem] = true;
    g[6][stem] = true;
    g
}

/// Draws `text` with dots `k` pixels square at (x, y), anti-aliased by 4x4 supersampling over what
/// is already there. Returns the box the ink spans.
fn draw_text(f: &mut Frame, x: u32, y: u32, k: u32, text: &str, fg: Rgb) -> Rect {
    let mut cursor = x;
    for ch in text.bytes() {
        if ch == b' ' {
            cursor += 4 * k;
            continue;
        }
        let g = glyph(ch);
        for gy in 0..7 * k {
            for gx in 0..5 * k {
                let mut on = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let (dx, dy) = ((gx * 4 + sx) / (4 * k), (gy * 4 + sy) / (4 * k));
                        on += u32::from(g[dy as usize][dx as usize]);
                    }
                }
                if on > 0 {
                    let (px, py) = (cursor + gx, y + gy);
                    let under = get(f, px, py);
                    set(f, px, py, blend(under, fg, f64::from(on) / 16.0));
                }
            }
        }
        cursor += 6 * k;
    }
    Rect { x, y, width: cursor - 6 * k - x + 5 * k, height: 7 * k }
}

/// Draws a hollow square `w` wide with a `t` pixel border: a checkbox or an icon's outline.
fn outline(f: &mut Frame, x: u32, y: u32, w: u32, t: u32, c: Rgb) {
    fill_rect(f, x, y, w, t, c);
    fill_rect(f, x, y + w - t, w, t, c);
    fill_rect(f, x, y, t, w, c);
    fill_rect(f, x + w - t, y, t, w, c);
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn region_in(report: &Report, rect: Rect) -> Vec<&Region> {
    report.regions.iter().filter(|r| overlaps(r.rect, rect)).collect()
}

// ---- the ratio maths --------------------------------------------------------------------------

#[test]
fn black_on_white_is_21_to_1() {
    assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 1e-9);
    assert!((contrast_ratio([255, 255, 255], [0, 0, 0]) - 21.0).abs() < 1e-9);
}

#[test]
fn a_colour_against_itself_is_1_to_1() {
    assert!((contrast_ratio(rgb(0x3366cc), rgb(0x3366cc)) - 1.0).abs() < 1e-12);
}

#[test]
fn the_well_known_grey_on_white_pairs() {
    // #777 on white is the textbook "just fails 4.5:1"; #767676 is the lightest grey that passes.
    let r = contrast_ratio(rgb(0x777777), rgb(0xffffff));
    assert!((r - 4.478).abs() < 0.005, "{r}");
    assert!(r < NORMAL_TEXT);
    let r = contrast_ratio(rgb(0x767676), rgb(0xffffff));
    assert!((r - 4.542).abs() < 0.005, "{r}");
    assert!(r >= NORMAL_TEXT);
    // #595959 on white is 7:1, the AAA ratio.
    assert!((contrast_ratio(rgb(0x595959), rgb(0xffffff)) - 7.0).abs() < 0.01);
}

#[test]
fn luminance_has_the_wcag_coefficients() {
    assert!((luminance([255, 0, 0]) - 0.2126).abs() < 1e-9);
    assert!((luminance([0, 255, 0]) - 0.7152).abs() < 1e-9);
    assert!((luminance([0, 0, 255]) - 0.0722).abs() < 1e-9);
    // Below the linear segment's knee the value is divided by 12.92.
    assert!((luminance([10, 10, 10]) - 10.0 / 255.0 / 12.92).abs() < 1e-9);
}

#[test]
fn colours_print_as_hex() {
    assert_eq!(hex([0x77, 0x07, 0xff]), "#7707ff");
}

// ---- known pairs, several sizes ----------------------------------------------------------------

/// A white-or-`bg` frame with one line of `k`-dot text, and the line's box.
fn one_line(bg: Rgb, fg: Rgb, k: u32) -> (Frame, Rect) {
    let mut f = solid(520, 200, bg);
    let rect = draw_text(&mut f, 60, 70, k, "Settings Label", fg);
    (f, rect)
}

fn expect_line(bg: Rgb, fg: Rgb, k: u32, scale: f64) -> Vec<Region> {
    let (f, rect) = one_line(bg, fg, k);
    let report = check(&f, &Options { region: None, scale });
    let found: Vec<Region> = region_in(&report, rect).into_iter().copied().collect();
    assert!(!found.is_empty(), "no region for {fg:?} on {bg:?} at k={k}");
    assert_eq!(report.regions.len(), found.len(), "a region off the text for {fg:?} on {bg:?} at k={k}: {:?}", report.regions);
    found
}

#[test]
fn text_below_4_5_to_1_is_a_finding_at_every_size() {
    let want = contrast_ratio(rgb(0x777777), rgb(0xffffff));
    // k=2 is 14 px of ink, k=3 21 px, at 2x scale: 7 and 10.5 pt, normal text.
    for k in [2, 3, 4] {
        for r in expect_line(rgb(0xffffff), rgb(0x777777), k, 2.0) {
            assert_eq!(r.kind, Kind::Text, "k={k}");
            assert!(r.fails(), "k={k}: {r:?}");
            assert!((r.ratio - want).abs() < 0.1, "k={k}: {} vs {want}", r.ratio);
            assert_eq!(r.foreground, rgb(0x777777), "k={k}");
            assert_eq!(r.background, rgb(0xffffff), "k={k}");
        }
    }
}

#[test]
fn text_at_4_5_to_1_or_better_is_not_a_finding() {
    for (fg, bg) in [(0x767676, 0xffffff), (0x000000, 0xffffff), (0x595959, 0xffffff), (0xffffff, 0x000000), (0x8e8e93, 0x000000)] {
        for k in [2, 3, 5] {
            for r in expect_line(rgb(bg), rgb(fg), k, 2.0) {
                assert!(!r.fails(), "{fg:06x} on {bg:06x} at k={k} flagged: {r:?}");
                assert!((r.ratio - contrast_ratio(rgb(fg), rgb(bg))).abs() < 0.1, "{fg:06x} on {bg:06x}: {}", r.ratio);
            }
        }
    }
}

#[test]
fn faint_and_dark_mode_failures_are_found() {
    // (foreground, background, the ratio it measures): light greys, dim text on black, light text
    // on a mid blue.
    for (fg, bg) in [(0x999999, 0xffffff), (0xcccccc, 0xffffff), (0x444444, 0x000000), (0xffffff, 0x0a84ff), (0x6e6e73, 0x1c1c1e)] {
        let want = contrast_ratio(rgb(fg), rgb(bg));
        assert!(want < NORMAL_TEXT);
        for r in expect_line(rgb(bg), rgb(fg), 3, 2.0) {
            assert!(r.fails(), "{fg:06x} on {bg:06x}: {r:?}");
            assert!((r.ratio - want).abs() / want < 0.05, "{fg:06x} on {bg:06x}: {} vs {want}", r.ratio);
        }
    }
}

#[test]
fn large_text_only_has_to_reach_3_to_1() {
    // k=4 at scale 1 is 28 pt of ink: large. #777 (4.48) passes; #aaa (2.32) doesn't.
    for r in expect_line(rgb(0xffffff), rgb(0x777777), 4, 1.0) {
        assert_eq!(r.kind, Kind::LargeText);
        assert!(!r.fails(), "{r:?}");
    }
    for r in expect_line(rgb(0xffffff), rgb(0xaaaaaa), 4, 1.0) {
        assert_eq!(r.kind, Kind::LargeText);
        assert!(r.fails(), "{r:?}");
    }
}

#[test]
fn the_scale_decides_what_is_large() {
    // The same pixels: 28 px of ink is large at 1 px a point and small text at 3 px a point.
    let at = |scale| expect_line(rgb(0xffffff), rgb(0x777777), 4, scale)[0];
    assert_eq!(at(1.0).kind, Kind::LargeText);
    assert_eq!(at(3.0).kind, Kind::Text);
    assert!(at(3.0).fails() && !at(1.0).fails());
}

#[test]
fn a_squarish_graphic_is_a_component_held_to_3_to_1() {
    let mut f = solid(300, 300, rgb(0xffffff));
    outline(&mut f, 100, 100, 72, 6, rgb(0xcccccc));
    let report = check(&f, &Options { region: None, scale: 3.0 });
    let found = region_in(&report, Rect { x: 100, y: 100, width: 72, height: 72 });
    assert_eq!(found.len(), 1, "{:?}", report.regions);
    assert_eq!(found[0].kind, Kind::Component);
    assert!(found[0].fails());
    assert!((found[0].ratio - contrast_ratio(rgb(0xcccccc), rgb(0xffffff))).abs() < 0.05);
    // The same checkbox at #8a8a8a (3.5:1) passes.
    let mut f = solid(300, 300, rgb(0xffffff));
    outline(&mut f, 100, 100, 72, 6, rgb(0x8a8a8a));
    let report = check(&f, &Options { region: None, scale: 3.0 });
    assert!(report.regions.iter().all(|r| !r.fails()), "{:?}", report.regions);
    assert!(!report.regions.is_empty());
}

#[test]
fn light_text_on_a_filled_button_is_judged_against_the_button() {
    let mut f = solid(520, 260, rgb(0xffffff));
    fill_rect(&mut f, 60, 60, 400, 120, rgb(0x0a84ff));
    let rect = draw_text(&mut f, 150, 100, 3, "Continue", rgb(0xffffff));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let found = region_in(&report, rect);
    assert_eq!(found.len(), 1, "{:?}", report.regions);
    assert_eq!(found[0].foreground, rgb(0xffffff));
    assert_eq!(found[0].background, rgb(0x0a84ff));
    assert!(found[0].fails(), "white on #0a84ff is 3.7:1");
    assert_eq!(report.regions.len(), 1, "the filled button isn't a finding: {:?}", report.regions);
}

// ---- gradients ---------------------------------------------------------------------------------

#[test]
fn a_plain_gradient_has_no_text() {
    for vertical in [true, false] {
        let f = gradient(600, 600, rgb(0xffffff), rgb(0x303030), vertical);
        assert_eq!(check(&f, &Options::default()).regions, []);
    }
}

#[test]
fn text_on_a_gradient_is_judged_against_its_worst_background() {
    // White to #d0d0d0 left to right; #767676 text is 4.54:1 on the white end and less on the grey.
    let mut f = gradient(600, 240, rgb(0xffffff), rgb(0xd0d0d0), false);
    let rect = draw_text(&mut f, 100, 100, 3, "Gradient Label", rgb(0x767676));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let found = region_in(&report, rect);
    assert_eq!(found.len(), 1, "{:?}", report.regions);
    let r = found[0];
    assert_eq!(r.foreground, rgb(0x767676));
    assert!(r.fails(), "{r:?}");
    let darkest = blend(rgb(0xffffff), rgb(0xd0d0d0), f64::from(rect.x + rect.width) / 599.0);
    let worst = contrast_ratio(rgb(0x767676), darkest);
    assert!(r.ratio < 4.54 && (r.ratio - worst).abs() / worst < 0.1, "{} vs the box's darkest end {worst}", r.ratio);
}

#[test]
fn dark_text_on_a_gradient_passes() {
    let mut f = gradient(600, 240, rgb(0xffffff), rgb(0xd0d0d0), true);
    let rect = draw_text(&mut f, 100, 100, 3, "Gradient Label", rgb(0x222222));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let found = region_in(&report, rect);
    assert_eq!(found.len(), 1, "{:?}", report.regions);
    assert!(!found[0].fails(), "{:?}", found[0]);
}

// ---- what isn't text ---------------------------------------------------------------------------

#[test]
fn dividers_and_panels_are_not_text() {
    let mut f = solid(600, 600, rgb(0xf2f2f7));
    // A grouped list cell (white on #f2f2f7 is 1.1:1 and that is by design), its hairline, and a
    // 1 pt scroll bar.
    fill_rect(&mut f, 30, 100, 540, 300, rgb(0xffffff));
    fill_rect(&mut f, 60, 250, 510, 2, rgb(0xd1d1d6));
    fill_rect(&mut f, 590, 50, 4, 400, rgb(0xc7c7cc));
    assert_eq!(check(&f, &Options { region: None, scale: 2.0 }).regions, []);
}

/// A frame of `amp` wide noise around `base`, per channel or grey.
fn noise(width: u32, height: u32, base: u8, amp: u8, grey: bool, seed: u64) -> Frame {
    let mut rng = Rng(seed);
    let mut f = solid(width, height, [base; 3]);
    let wiggle = |rng: &mut Rng| i32::from(rng.byte()) * i32::from(amp) / 255 - i32::from(amp) / 2;
    for y in 0..height {
        for x in 0..width {
            let g = wiggle(&mut rng);
            let c = std::array::from_fn(|_| (i32::from(base) + if grey { g } else { wiggle(&mut rng) }).clamp(0, 255) as u8);
            set(&mut f, x, y, c);
        }
    }
    f
}

/// Smooth blobs plus grain: what a photo looks like to an edge detector.
fn photo(width: u32, height: u32, seed: u64) -> Frame {
    let mut rng = Rng(seed);
    let waves: Vec<[f64; 5]> = (0..6).map(|_| std::array::from_fn(|_| f64::from(rng.byte()) / 255.0)).collect();
    let mut f = solid(width, height, [0; 3]);
    for y in 0..height {
        for x in 0..width {
            let c: [u8; 3] = std::array::from_fn(|ch| {
                let v: f64 = waves
                    .iter()
                    .enumerate()
                    .map(|(i, w)| {
                        ((f64::from(x) * (0.01 + w[0] * 0.08) + f64::from(y) * (0.01 + w[1] * 0.08) + w[2] * 6.0 + (i + ch) as f64).sin())
                            * (0.1 + w[3] * 0.15)
                    })
                    .sum();
                let grain = (f64::from(rng.byte()) / 255.0 - 0.5) * 18.0;
                (128.0 + v * 140.0 + grain).clamp(0.0, 255.0) as u8
            });
            set(&mut f, x, y, c);
        }
    }
    f
}

#[test]
fn noise_and_photos_are_not_text() {
    // False positives over every noise-like frame: none allowed.
    let mut checked = 0;
    let mut false_positives = 0;
    for seed in 1..=4u64 {
        for (amp, grey) in [(255, true), (255, false), (80, true), (40, false), (16, true), (8, false)] {
            let f = noise(500, 700, 128, amp, grey, seed * 7919 + u64::from(amp));
            false_positives += check(&f, &Options { region: None, scale: 2.0 }).regions.len();
            checked += 1;
        }
        let f = photo(500, 700, seed * 104_729);
        false_positives += check(&f, &Options { region: None, scale: 2.0 }).regions.len();
        checked += 1;
    }
    println!("{checked} noise and photo-like frames: {false_positives} false positives");
    assert_eq!(false_positives, 0);
}

#[test]
fn text_next_to_a_photo_is_still_found() {
    let mut f = solid(700, 500, rgb(0xffffff));
    let pic = photo(300, 300, 31);
    for y in 0..300 {
        for x in 0..300 {
            set(&mut f, 360 + x, 100 + y, get(&pic, x, y));
        }
    }
    let rect = draw_text(&mut f, 40, 200, 3, "Caption", rgb(0x999999));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let found = region_in(&report, rect);
    assert_eq!(found.len(), 1, "{:?}", report.regions);
    assert!(found[0].fails());
    assert!(report.regions.iter().all(|r| !overlaps(r.rect, Rect { x: 360, y: 100, width: 300, height: 300 })), "{:?}", report.regions);
}

// ---- options and output ------------------------------------------------------------------------

#[test]
fn a_region_limits_the_check_and_keeps_frame_coordinates() {
    let mut f = solid(520, 300, rgb(0xffffff));
    let top = draw_text(&mut f, 60, 40, 3, "Top line", rgb(0x999999));
    let bottom = draw_text(&mut f, 60, 200, 3, "Bottom line", rgb(0xaaaaaa));
    let both = check(&f, &Options { region: None, scale: 2.0 });
    assert_eq!((region_in(&both, top).len(), region_in(&both, bottom).len()), (1, 1));
    let only = check(&f, &Options { region: Some(Rect { x: 0, y: 150, width: 520, height: 150 }), scale: 2.0 });
    assert_eq!(only.regions.len(), 1, "{:?}", only.regions);
    assert!(overlaps(only.regions[0].rect, bottom));
    assert_eq!(only.regions[0].rect, both.regions.iter().find(|r| overlaps(r.rect, bottom)).expect("bottom").rect);
    // A region past the frame, or empty, finds nothing and doesn't panic.
    assert_eq!(check(&f, &Options { region: Some(Rect { x: 900, y: 900, width: 50, height: 50 }), scale: 2.0 }).regions, []);
    assert_eq!(check(&f, &Options { region: Some(Rect { x: 0, y: 0, width: 0, height: 10 }), scale: 2.0 }).regions, []);
}

#[test]
fn regions_come_top_to_bottom() {
    let mut f = solid(520, 400, rgb(0xffffff));
    for (i, y) in [60u32, 160, 260].into_iter().enumerate() {
        draw_text(&mut f, 60, y, 3, "Row of text", rgb(0x999999 - i as u32 * 0x111111));
    }
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let ys: Vec<u32> = report.regions.iter().map(|r| r.rect.y).collect();
    assert_eq!(ys.len(), 3, "{:?}", report.regions);
    assert!(ys.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn a_degenerate_scale_is_treated_as_one_pixel_a_point() {
    let (f, rect) = one_line(rgb(0xffffff), rgb(0x999999), 3);
    let want = check(&f, &Options { region: None, scale: 1.0 });
    for scale in [0.0, -2.0, f64::NAN, f64::INFINITY] {
        assert_eq!(check(&f, &Options { region: None, scale }), want, "scale {scale}");
    }
    assert!(!region_in(&want, rect).is_empty());
}

#[test]
fn a_short_buffer_finds_nothing() {
    let f = Frame { width: 100, height: 100, rgba: vec![0; 40] };
    assert_eq!(check(&f, &Options::default()).regions, []);
}

#[test]
fn annotating_boxes_the_findings_and_leaves_the_text_alone() {
    let (f, text) = one_line(rgb(0xffffff), rgb(0x999999), 3);
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let finding = report.findings().next().expect("a finding").rect;
    assert!(overlaps(finding, text));
    let out = annotate(&f, report.findings());
    assert_eq!((out.width, out.height), (f.width, f.height));
    // A red pixel just outside the box's top-left corner and the middle of its top edge.
    assert_eq!(get(&out, finding.x - 1, finding.y - 1), [255, 0, 0]);
    assert_eq!(get(&out, finding.x + finding.width / 2, finding.y - 1), [255, 0, 0]);
    // Inside the box, the pixels are the measured ones.
    for y in finding.y..finding.y + finding.height {
        for x in finding.x..finding.x + finding.width {
            assert_eq!(get(&out, x, y), get(&f, x, y));
        }
    }
    // Nothing is drawn far away, and with no findings the frame is unchanged.
    assert_eq!(get(&out, 5, 5), get(&f, 5, 5));
    assert_eq!(annotate(&f, std::iter::empty()), f);
    // It encodes as a PNG, which a client can show.
    let png = crate::frame::encode_png(&out).expect("png");
    assert_eq!(crate::frame::png_size(&png), Some((f.width, f.height)));
}

#[test]
fn boxes_at_the_frame_edge_are_clipped() {
    let mut f = solid(120, 80, rgb(0xffffff));
    let rect = draw_text(&mut f, 0, 0, 2, "Edge", rgb(0x999999));
    let region = Region { rect, kind: Kind::Text, foreground: [0; 3], background: [255; 3], ratio: 2.0 };
    let out = annotate(&f, [&region]);
    assert_eq!((out.width, out.height), (120, 80));
}

// ---- a labelled screen -------------------------------------------------------------------------

/// What the labelled screen holds: (text, ink box, foreground, whether it should be a finding).
struct Label {
    rect: Rect,
    fail: bool,
}

/// An iPhone-sized (3 px a point) settings-like screen: a title, grouped cells with labels and
/// secondary text of known colours, dividers, a filled button, a checkbox icon and a photo.
fn labelled_screen() -> (Frame, Vec<Label>) {
    let (w, h) = (1206u32, 2622u32);
    let mut f = solid(w, h, rgb(0xf2f2f7));
    let labels = std::cell::RefCell::new(Vec::new());
    let label = |f: &mut Frame, x, y, k, text: &str, fg: u32, bg: u32| {
        let rect = draw_text(f, x, y, k, text, rgb(fg));
        labels.borrow_mut().push(Label { rect, fail: contrast_ratio(rgb(fg), rgb(bg)) < if k >= 8 { LARGE_TEXT } else { NORMAL_TEXT } });
    };
    // Title: 34 pt-ish large text.
    label(&mut f, 60, 200, 14, "Settings", 0x000000, 0xf2f2f7);
    label(&mut f, 60, 380, 14, "Account", 0xb0b0b8, 0xf2f2f7);
    // Three grouped cells on white.
    for (i, (primary, secondary)) in [(0x000000, 0x8e8e93), (0x1c1c1e, 0xb4b4b9), (0x333333, 0x767676)].into_iter().enumerate() {
        let top = 560 + i as u32 * 260;
        fill_rect(&mut f, 40, top, 1126, 240, rgb(0xffffff));
        label(&mut f, 100, top + 50, 5, "Notifications", primary, 0xffffff);
        label(&mut f, 100, top + 140, 4, "Banners and sounds", secondary, 0xffffff);
        if i < 2 {
            fill_rect(&mut f, 100, top + 240, 1066, 2, rgb(0xc6c6c8));
        }
    }
    // Secondary text on the grouped background (#f2f2f7), light.
    label(&mut f, 100, 1400, 4, "Footer note about privacy", 0x8e8e93, 0xf2f2f7);
    label(&mut f, 100, 1500, 4, "Another footer note here", 0xaeaeb2, 0xf2f2f7);
    // A filled button with white text: 3.7:1, a failing label; a dark one passes.
    fill_rect(&mut f, 60, 1700, 1086, 160, rgb(0x0a84ff));
    label(&mut f, 400, 1745, 10, "Continue", 0xffffff, 0x0a84ff);
    fill_rect(&mut f, 60, 1900, 1086, 160, rgb(0x0040dd));
    label(&mut f, 400, 1945, 10, "Confirm", 0xffffff, 0x0040dd);
    // A checkbox outline, light (a failing component), beside its label.
    outline(&mut f, 100, 2150, 96, 8, rgb(0xd0d0d0));
    labels.borrow_mut().push(Label { rect: Rect { x: 100, y: 2150, width: 96, height: 96 }, fail: true });
    label(&mut f, 240, 2170, 4, "Remember me", 0x000000, 0xf2f2f7);
    // A photo.
    let pic = photo(1000, 260, 77);
    for y in 0..260 {
        for x in 0..1000 {
            set(&mut f, 100 + x, 2300 + y, get(&pic, x, y));
        }
    }
    (f, labels.into_inner())
}

#[test]
fn a_labelled_screen_has_every_failing_label_found_and_no_false_positives() {
    let (f, labels) = labelled_screen();
    let report = check(&f, &Options { region: None, scale: 3.0 });
    let findings: Vec<&Region> = report.findings().collect();
    let expected: Vec<&Label> = labels.iter().filter(|l| l.fail).collect();
    let found = expected.iter().filter(|l| findings.iter().any(|r| overlaps(r.rect, l.rect))).count();
    let false_positives: Vec<&&Region> = findings.iter().filter(|r| !expected.iter().any(|l| overlaps(r.rect, l.rect))).collect();
    println!(
        "labelled screen: {} labels, {} failing; found {found}, {} findings, {} false positives",
        labels.len(),
        expected.len(),
        findings.len(),
        false_positives.len()
    );
    assert_eq!(found, expected.len(), "missed a failing label: {findings:?}");
    assert!(false_positives.is_empty(), "{false_positives:?}");
    // One finding per failing label: a line's glyphs and words are joined, counters and dots dropped.
    assert_eq!(findings.len(), expected.len(), "{findings:?}");
    // Passing labels were evaluated and passed, not skipped: every label has a region.
    let passing: Vec<&Label> = labels.iter().filter(|l| !l.fail).collect();
    assert!(passing.iter().all(|l| report.regions.iter().any(|r| overlaps(r.rect, l.rect) && !r.fails())), "a passing label has no region");
}

// ---- boxes and regions (#221) --------------------------------------------------------------------

/// A white frame with a field: a 2 pixel `border` round a white fill, and `text` inside.
fn field(border: Rgb, text: Rgb) -> (Frame, Rect) {
    let mut f = solid(520, 260, rgb(0xffffff));
    fill_rect(&mut f, 60, 60, 300, 90, border);
    fill_rect(&mut f, 62, 62, 296, 86, rgb(0xffffff));
    let rect = draw_text(&mut f, 90, 90, 3, "Email", text);
    (f, rect)
}

#[test]
fn a_border_does_not_hide_failing_text_inside_it() {
    let (f, text) = field(rgb(0xc6c6c8), rgb(0x8e8e93));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let inside = Rect { x: 62, y: 62, width: 296, height: 86 };
    let found: Vec<&Region> = region_in(&report, text).into_iter().filter(|r| contains(inside, r.rect)).collect();
    assert!(found.iter().any(|r| r.fails() && r.foreground == rgb(0x8e8e93) && r.background == rgb(0xffffff)), "{:?}", report.regions);
}

#[test]
fn a_border_is_judged_as_a_component_not_as_text() {
    let (f, _) = field(rgb(0xc6c6c8), rgb(0x000000));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let border: Vec<&Region> = report.regions.iter().filter(|r| r.rect.width >= 290).collect();
    assert_eq!(border.len(), 1, "{:?}", report.regions);
    assert_eq!(border[0].kind, Kind::Component);
    assert!(border[0].fails(), "#c6c6c8 on white is 1.8:1, under 3:1");
    // Dark text in the same box passes.
    assert!(report.regions.iter().filter(|r| r.rect.width < 290).all(|r| !r.fails()), "{:?}", report.regions);
}

#[test]
fn a_dark_border_and_dark_text_pass() {
    let (f, _) = field(rgb(0x555555), rgb(0x111111));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    assert!(report.findings().next().is_none(), "{:?}", report.regions);
}

#[test]
fn a_region_drawn_tightly_round_a_label_checks_it() {
    let mut f = solid(520, 300, rgb(0xffffff));
    let word = draw_text(&mut f, 200, 120, 3, "Wordy", rgb(0x999999));
    let tight = Rect { x: word.x - 10, y: word.y - 10, width: word.width + 20, height: word.height + 20 };
    assert!(f64::from(word.width) * f64::from(word.height) > 0.3 * f64::from(tight.width) * f64::from(tight.height));
    let report = check(&f, &Options { region: Some(tight), scale: 2.0 });
    assert_eq!(report.findings().count(), 1, "{:?}", report.regions);
}

// ---- capsule outlines and thick borders (#233) --------------------------------------------------

/// Draws an antialiased outline: a rounded box `at` with corners of radius `r` and a `t` pixel border
/// of `c` round a `fill` inside, over what is there.
fn rounded(f: &mut Frame, at: Rect, r: f64, t: f64, c: Rgb, fill: Rgb) {
    let Rect { x, y, width: w, height: h } = at;
    let half = (f64::from(w) / 2.0, f64::from(h) / 2.0);
    for yy in 0..h {
        for xx in 0..w {
            let (px, py) = (f64::from(xx) + 0.5, f64::from(yy) + 0.5);
            // How far the pixel is inside (negative) or outside the rounded box.
            let (qx, qy) = ((px - half.0).abs() - (half.0 - r), (py - half.1).abs() - (half.1 - r));
            let sd = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
            let outer = (0.5 - sd).clamp(0.0, 1.0);
            let inner = (0.5 - (sd + t)).clamp(0.0, 1.0);
            let under = get(f, x + xx, y + yy);
            set(f, x + xx, y + yy, blend(blend(under, c, outer), fill, inner));
        }
    }
}

/// A capsule: rounded ends of radius half the shorter side (a circle when the box is square).
fn capsule(f: &mut Frame, at: Rect, t: f64, c: Rgb, fill: Rgb) {
    rounded(f, at, f64::from(at.width.min(at.height)) / 2.0, t, c, fill);
}

/// The text the frame rect must not hide: the box `rect` is the inside of the outline.
fn failing_text_inside(f: &Frame, text: Rect, inside: Rect, scale: f64) -> Report {
    let report = check(f, &Options { region: None, scale });
    let found: Vec<&Region> = region_in(&report, text).into_iter().filter(|r| contains(inside, r.rect)).collect();
    assert!(found.iter().any(|r| r.fails() && r.foreground == rgb(0x8e8e93) && r.background == rgb(0xffffff)), "{:?}", report.regions);
    report
}

/// The regions as wide as a whole box: a box's own finding, not its text.
fn boxes(report: &Report, width: u32) -> Vec<&Region> {
    report.regions.iter().filter(|r| r.rect.width >= width).collect()
}

#[test]
fn a_capsule_outline_does_not_hide_failing_text_inside_it() {
    // Material 3's outlined button: a 3 px #c6c6c8 pill on white.
    let mut f = solid(520, 260, rgb(0xffffff));
    capsule(&mut f, Rect { x: 60, y: 60, width: 300, height: 90 }, 3.0, rgb(0xc6c6c8), rgb(0xffffff));
    let text = draw_text(&mut f, 130, 90, 3, "Cancel", rgb(0x8e8e93));
    let report = failing_text_inside(&f, text, Rect { x: 65, y: 65, width: 290, height: 80 }, 3.0);
    // The pill's own outline is a component finding, 1.8:1 against what is outside it.
    let pill = boxes(&report, 290);
    assert_eq!(pill.len(), 1, "{:?}", report.regions);
    assert_eq!((pill[0].kind, pill[0].fails()), (Kind::Component, true));
}

#[test]
fn a_capsule_with_dark_text_and_a_dark_outline_passes() {
    let mut f = solid(520, 260, rgb(0xffffff));
    capsule(&mut f, Rect { x: 60, y: 60, width: 300, height: 90 }, 3.0, rgb(0x555555), rgb(0xffffff));
    draw_text(&mut f, 130, 90, 3, "Cancel", rgb(0x111111));
    let report = check(&f, &Options { region: None, scale: 3.0 });
    assert!(report.findings().next().is_none(), "{:?}", report.regions);
}

#[test]
fn a_six_pixel_border_does_not_hide_failing_text_inside_it() {
    // A 2 dp focus ring at 3x.
    let mut f = solid(520, 260, rgb(0xffffff));
    fill_rect(&mut f, 60, 60, 300, 90, rgb(0xc6c6c8));
    fill_rect(&mut f, 66, 66, 288, 78, rgb(0xffffff));
    let text = draw_text(&mut f, 90, 90, 3, "Email", rgb(0x8e8e93));
    let report = failing_text_inside(&f, text, Rect { x: 66, y: 66, width: 288, height: 78 }, 3.0);
    assert_eq!(boxes(&report, 290).len(), 1, "{:?}", report.regions);
}

#[test]
fn a_field_with_small_rounded_corners_does_not_hide_failing_text_inside_it() {
    // Material's 4 dp corner at 2x, a 1 to 2 px border: neither corner pair may read as square.
    for (t, r) in [(1.0, 6.0), (2.0, 8.0), (2.0, 9.0)] {
        let mut f = solid(520, 260, rgb(0xffffff));
        rounded(&mut f, Rect { x: 60, y: 60, width: 300, height: 90 }, r, t, rgb(0xc6c6c8), rgb(0xffffff));
        let text = draw_text(&mut f, 90, 90, 3, "Email", rgb(0x8e8e93));
        let report = failing_text_inside(&f, text, Rect { x: 62, y: 62, width: 296, height: 86 }, 2.0);
        assert_eq!(boxes(&report, 290).len(), 1, "border {t}, radius {r}: {:?}", report.regions);
    }
}

#[test]
fn a_small_ring_or_o_is_a_letter_not_a_frame() {
    // A lone grey O or 0 of 20 to 40 px is text of 3.26:1, under 4.5:1; read as a box it would
    // be a 3:1 component and pass.
    for d in [20, 28, 40] {
        for t in [2.0, 3.0] {
            for (w, h) in [(d, d), (d * 7 / 10, d)] {
                let mut f = solid(200, 120, rgb(0xffffff));
                capsule(&mut f, Rect { x: 60, y: 30, width: w, height: h }, t, rgb(0x8e8e93), rgb(0xffffff));
                let report = check(&f, &Options { region: None, scale: 3.0 });
                assert!(report.regions.iter().all(|r| r.kind != Kind::Component), "{w}x{h} stroke {t}: {:?}", report.regions);
                assert_eq!(report.findings().count(), 1, "{w}x{h} stroke {t}: {:?}", report.regions);
            }
        }
    }
}

/// A white card on white with a 1 px `edge` border, and a `shadow` `off` px down (none when `off` is 0, the
/// shadow then a 1 px ring round the card), with `text` inside. Returns the frame and the text's box.
fn card(radius: f64, edge: u8, shadow: u8, off: u32, text: Rgb) -> (Frame, Rect) {
    let mut f = solid(520, 260, rgb(0xffffff));
    let g = |v: u8| [v, v, v];
    let at = Rect { x: 60, y: 60, width: 300, height: 90 };
    if off == 0 {
        rounded(
            &mut f,
            Rect { x: at.x - 1, y: at.y - 1, width: at.width + 2, height: at.height + 2 },
            radius + 1.0,
            1.0,
            g(shadow),
            rgb(0xffffff),
        );
    } else {
        rounded(&mut f, Rect { y: at.y + off, ..at }, radius, 1.0, g(shadow), g(shadow));
    }
    rounded(&mut f, at, radius, 1.0, g(edge), rgb(0xffffff));
    let text = draw_text(&mut f, 90, 90, 3, "Email", text);
    (f, text)
}

#[test]
fn a_cards_drop_shadow_does_not_hide_failing_text_inside_it() {
    // The shadow darkens the bottom edge (#eeeeee on the top and sides, #e0e0e0 below).
    for radius in [0.0, 2.0, 4.0, 8.0] {
        for off in 0..=3 {
            let (f, text) = card(radius, 0xee, 0xe0, off, rgb(0x8e8e93));
            failing_text_inside(&f, text, Rect { x: 62, y: 62, width: 296, height: 86 }, 2.0);
        }
    }
}

#[test]
fn a_shadowed_card_with_dark_text_passes_and_its_edge_is_its_own_finding() {
    let (f, _) = card(0.0, 0xee, 0xe0, 2, rgb(0x111111));
    let report = check(&f, &Options { region: None, scale: 2.0 });
    let edge = boxes(&report, 290);
    assert_eq!(edge.len(), 1, "{:?}", report.regions);
    assert_eq!(edge[0].kind, Kind::Component);
    assert!(report.findings().all(|r| r.rect.width >= 290), "the text passes: {:?}", report.regions);
}

#[test]
fn what_a_border_shadow_and_text_sweep_finds_is_what_the_text_says() {
    // Light borders and shadows of several strengths round boxes of several sizes: the grey text
    // inside is a finding in every one, never swallowed by the box.
    for (w, h) in [(300, 90), (200, 60), (130, 44)] {
        for radius in [0.0, 2.0, 4.0, 8.0] {
            for (edge, shadow) in [(0xee, 0xe0), (0xee, 0xee), (0xe6, 0xc8), (0xf5, 0xe0), (0xe8, 0xd8), (0xf0, 0xd2)] {
                for off in 0..=3 {
                    let mut f = solid(520, 260, rgb(0xffffff));
                    let g = |v: u8| [v, v, v];
                    let at = Rect { x: 60, y: 60, width: w, height: h };
                    rounded(&mut f, Rect { y: at.y + off, ..at }, radius, 1.0, g(shadow), g(shadow));
                    rounded(&mut f, at, radius, 1.0, g(edge), rgb(0xffffff));
                    let text = draw_text(&mut f, 80, 60 + h / 2 - 10, 3, "Em", rgb(0x8e8e93));
                    let inside = Rect { x: 62, y: 62, width: w - 4, height: h - 4 };
                    failing_text_inside(&f, text, inside, 2.0);
                }
            }
        }
    }
}

/// The error function, to 1.5e-7 (Abramowitz and Stegun 7.1.26).
fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y =
        1.0 - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t + 0.254_829_592) * t * (-x * x).exp();
    y.copysign(x)
}

/// A white card on `page` with a blurred drop shadow (a Gaussian of `sigma` px, black at `alpha`, `dy` px
/// down), and `text` inside.
fn soft_card(page: u8, sigma: f64, alpha: f64, dy: f64, text: Rgb) -> (Frame, Rect) {
    let mut f = solid(520, 260, [page, page, page]);
    let (x0, y0, x1, y1) = (60.0, 60.0, 360.0, 150.0);
    let cover = |p: f64, a: f64, b: f64| 0.5 * (erf((p - a) / (sigma * 2f64.sqrt())) - erf((p - b) / (sigma * 2f64.sqrt())));
    for y in 20..220 {
        for x in 20..400 {
            let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let dark = alpha * cover(px, x0, x1) * cover(py, y0 + dy, y1 + dy);
            let under = f64::from(page) * (1.0 - dark);
            let v = if (x0..x1).contains(&px) && (y0..y1).contains(&py) { 255 } else { under.round() as u8 };
            set(&mut f, x, y, [v, v, v]);
        }
    }
    let text = draw_text(&mut f, 90, 90, 3, "Email", text);
    (f, text)
}

#[test]
fn a_card_on_an_off_white_page_with_a_soft_shadow_does_not_hide_failing_text() {
    // The card is lighter than the page and the shadow darker: the border's first pixel is shadow and
    // the next is the card, on the other side of the page's colour.
    for page in [0xfa, 0xf8, 0xf5, 0xff] {
        for (sigma, alpha, dy) in [(4.0, 0.3, 3.0), (3.0, 0.25, 2.0), (6.0, 0.4, 4.0), (2.0, 0.3, 1.0)] {
            let (f, text) = soft_card(page, sigma, alpha, dy, rgb(0x8e8e93));
            failing_text_inside(&f, text, Rect { x: 62, y: 62, width: 296, height: 86 }, 2.0);
        }
    }
}

/// Draws an antialiased elliptical outline filling `at`, a `t` pixel stroke of `c` round a `fill` inside.
fn ellipse(f: &mut Frame, at: Rect, t: f64, c: Rgb, fill: Rgb) {
    let (a, b) = (f64::from(at.width) / 2.0, f64::from(at.height) / 2.0);
    for yy in 0..at.height {
        for xx in 0..at.width {
            let (px, py) = (f64::from(xx) + 0.5 - a, f64::from(yy) + 0.5 - b);
            let (k0, k1) = ((px / a).hypot(py / b), (px / (a * a)).hypot(py / (b * b)));
            let sd = if k1 > 0.0 { k0 * (k0 - 1.0) / k1 } else { -b.min(a) };
            let (outer, inner) = ((0.5 - sd).clamp(0.0, 1.0), (0.5 - (sd + t)).clamp(0.0, 1.0));
            let under = get(f, at.x + xx, at.y + yy);
            set(f, at.x + xx, at.y + yy, blend(blend(under, c, outer), fill, inner));
        }
    }
}

#[test]
fn a_bold_oval_wider_than_tall_is_a_letter_not_a_frame() {
    // A big O in a heavy weight: a 4 to 6 px stroke is thin enough next to the box, but its top and
    // bottom have no straight run.
    for w in [40, 50, 60, 70, 90, 120, 160] {
        for t in [4.0, 5.0, 6.0] {
            let mut f = solid(400, 240, rgb(0xffffff));
            ellipse(&mut f, Rect { x: 60, y: 30, width: w, height: w * 3 / 5 }, t, rgb(0x8e8e93), rgb(0xffffff));
            // At a scale that keeps it under 16 pt, where a squarish shape is a control, not text.
            let report = check(&f, &Options { region: None, scale: (f64::from(w) * 0.6 / 12.0).max(3.0) });
            assert!(report.regions.iter().all(|r| r.kind != Kind::Component), "{w} stroke {t}: {:?}", report.regions);
            assert_eq!(report.findings().count(), 1, "{w} stroke {t}: {:?}", report.regions);
        }
    }
}

#[test]
fn a_thick_border_stays_thin_next_to_its_box() {
    // The same border on a box too small to call it thin is a filled shape, not a frame.
    let mut f = solid(200, 120, rgb(0xffffff));
    fill_rect(&mut f, 40, 40, 40, 30, rgb(0xc6c6c8));
    fill_rect(&mut f, 46, 46, 28, 18, rgb(0xffffff));
    draw_text(&mut f, 50, 50, 1, "A", rgb(0x8e8e93));
    let report = check(&f, &Options { region: None, scale: 3.0 });
    assert!(report.regions.iter().all(|r| r.foreground != rgb(0xc6c6c8)), "{:?}", report.regions);
}

#[test]
fn what_is_not_a_hollow_box_is_not_a_frame() {
    let mut f = solid(560, 700, rgb(0xffffff));
    // A solid button: a purple pill with white text.
    capsule(&mut f, Rect { x: 60, y: 20, width: 300, height: 90 }, 45.0, rgb(0x6750a4), rgb(0x6750a4));
    draw_text(&mut f, 130, 50, 3, "Save", rgb(0xffffff));
    // A solid rectangle button.
    fill_rect(&mut f, 60, 130, 300, 90, rgb(0x6750a4));
    draw_text(&mut f, 130, 160, 3, "Save", rgb(0xffffff));
    // An underlined label, and a divider.
    let label = draw_text(&mut f, 60, 250, 3, "Learn more", rgb(0x222222));
    fill_rect(&mut f, label.x, label.y + label.height + 4, label.width, 3, rgb(0x222222));
    let divider = Rect { x: 60, y: 320, width: 400, height: 3 };
    fill_rect(&mut f, divider.x, divider.y, divider.width, divider.height, rgb(0xc6c6c8));
    // A progress bar: a track and its fill.
    let bar = Rect { x: 60, y: 350, width: 300, height: 16 };
    fill_rect(&mut f, bar.x, bar.y, bar.width, bar.height, rgb(0xe0e0e0));
    fill_rect(&mut f, 60, 350, 120, 16, rgb(0x6750a4));
    // An O glyph and a ring icon (a circle outline): hollow, but not boxes.
    let rings = [(60, 64), (260, 48), (360, 80)];
    for (x, d) in rings {
        capsule(&mut f, Rect { x, y: 400, width: d, height: d }, 6.0, rgb(0x333333), rgb(0xffffff));
        draw_text(&mut f, x + d / 2 - 5, 400 + d / 2 - 7, 2, "I", rgb(0x333333));
    }
    capsule(&mut f, Rect { x: 160, y: 400, width: 44, height: 64 }, 8.0, rgb(0x333333), rgb(0xffffff));
    draw_text(&mut f, 160 + 17, 400 + 25, 2, "I", rgb(0x333333));
    let report = check(&f, &Options { region: None, scale: 3.0 });
    // A frame is always a component: none of these is one, whatever its width.
    let underlined = Rect { y: label.y, height: label.height + 8, ..label };
    for (what, rect) in [
        ("solid pill", Rect { x: 60, y: 20, width: 300, height: 90 }),
        ("solid button", Rect { x: 60, y: 130, width: 300, height: 90 }),
        ("underline", underlined),
        ("divider", divider),
        ("progress bar", bar),
    ] {
        let found = region_in(&report, rect);
        assert!(found.iter().all(|r| r.kind != Kind::Component), "{what}: {found:?}");
    }
    // Whatever is drawn inside a hollow glyph is its counter, not a box's content: a frame would keep
    // it as a region of its own, a glyph's ink drops it.
    for (x, w, h) in [(60, 64, 64), (160, 44, 64), (260, 48, 48), (360, 80, 80)] {
        let inside = Rect { x: x + 8, y: 408, width: w - 16, height: h - 16 };
        let kept: Vec<&Region> = report.regions.iter().filter(|r| contains(inside, r.rect)).collect();
        assert!(kept.is_empty(), "{inside:?}: {kept:?}");
    }
}

/// Timing of the check on a 1206x2622 frame: `cargo test --release -p simee-core contrast_cost -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn contrast_cost() {
    let (f, _) = labelled_screen();
    let opts = Options { region: None, scale: 3.0 };
    let mut times: Vec<f64> = (0..9)
        .map(|_| {
            let start = std::time::Instant::now();
            let report = check(&f, &opts);
            std::hint::black_box(&report);
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    let noise = noise(1206, 2622, 128, 255, true, 5);
    let start = std::time::Instant::now();
    let regions = check(&noise, &opts).regions.len();
    println!("check, labelled 1206x2622 screen: median {:.1} ms (min {:.1}, max {:.1})", times[4], times[0], times[8]);
    println!("check, 1206x2622 full-range noise: {:.1} ms, {regions} regions", start.elapsed().as_secs_f64() * 1000.0);
    let start = std::time::Instant::now();
    let report = check(&f, &opts);
    let png = crate::frame::encode_png(&annotate(&f, report.findings())).expect("png");
    println!("check + annotated PNG ({} bytes): {:.1} ms", png.len(), start.elapsed().as_secs_f64() * 1000.0);
}
