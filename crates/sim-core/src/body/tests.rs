use std::collections::HashMap;

use super::*;
use crate::fold;
use crate::form_factor::{Kind, matrix};

/// How closely a drawn screen matches simee's pixels (aspect and px/ppi size): 0.1%.
const EXACT: f32 = 0.001;
/// How closely a screen matches the maker's rounded figure (6.27" sold as 6.3"): 3%.
const PUBLISHED: f32 = 0.03;

fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance * b.abs().max(1e-6)
}

fn kind(platform: Platform, form_factor: FormFactor) -> Kind {
    Kind::new(platform, form_factor).expect("supported")
}

/// An Apple device type as CoreSimulator reports it (screen in pixels).
fn apple_type(id: &str, name: &str, family: &str, (w, h): (u32, u32), scale: u32) -> DeviceTypeInfo {
    DeviceTypeInfo {
        identifier: format!("com.apple.CoreSimulator.SimDeviceType.{id}"),
        name: name.to_owned(),
        model: String::new(),
        product_family: family.to_owned(),
        screen_width: f64::from(w),
        screen_height: f64::from(h),
        screen_scale: f64::from(scale),
        platform: Platform::Ios,
        form_factor: crate::form_factor::apple_type_form_factor(&format!("com.apple.CoreSimulator.SimDeviceType.{id}"), family),
    }
}

/// The profile sim-android gives a new AVD following a foldable definition.
fn foldable_profile(d: &fold::FoldableDevice) -> Profile {
    let mut p = kind(Platform::Android, FormFactor::Foldable).profile().with_screen(d.lcd.0, d.lcd.1, Density::Dpi(d.lcd.2));
    if let (Some(f), Some(r)) = (&mut p.fold, d.hinge.folded) {
        (f.folded_width, f.folded_height) = (r.width, r.height);
    }
    p
}

#[test]
fn every_device_type_simee_lists_gets_a_body() {
    // Every kind's reference profile: a body for phones, tablets, foldables and watches.
    for (form_factor, platform, k) in matrix() {
        let Ok(k) = k else { continue };
        let body = for_profile(&k.profile());
        assert_eq!(body.is_some(), has_body(form_factor), "{form_factor:?} on {platform:?}");
        if let Some(b) = body {
            assert_eq!((b.platform, b.form_factor), (platform, form_factor));
        }
    }
    // The AVD form factors simee creates are rows of the table, marked generic.
    for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Foldable, FormFactor::Watch] {
        let b = for_profile(&kind(Platform::Android, f).profile()).expect("body");
        assert!(all().iter().any(|row| Arc::ptr_eq(row, &b)), "{f:?} is a table row");
        assert!(matches!(b.source, Source::Generic(_)), "{f:?}");
    }
    // The foldable definitions new AVDs follow get their published bodies.
    for d in fold::devices() {
        let b = for_android(d.id, &foldable_profile(&d)).expect("body");
        assert_eq!(b.id, d.id);
        assert!(matches!(b.source, Source::Published(_)), "{}", d.id);
    }
    // The Apple device types simee names, and a current line-up.
    for id in [
        "iPhone-17",
        "iPhone-17-Pro",
        "iPhone-17-Pro-Max",
        "iPhone-Air",
        "iPhone-16e",
        "iPhone-16",
        "iPhone-16-Plus",
        "iPhone-16-Pro",
        "iPhone-16-Pro-Max",
        "iPhone-SE-3rd-generation",
        "iPad-Pro-11-inch-M4",
        "iPad-Pro-13-inch-M4",
        "iPad-Air-11-inch-M3",
        "iPad-Air-13-inch-M3",
        "iPad-mini-A17-Pro",
        "iPad-A16",
        "Apple-Watch-Series-10-46mm",
        "Apple-Watch-Series-10-42mm",
        "Apple-Watch-Ultra-2-49mm",
        "Apple-Watch-SE-44mm-2nd-generation",
        "iPhone-Duo",
    ] {
        let b = get(&format!("com.apple.CoreSimulator.SimDeviceType.{id}")).unwrap_or_else(|| panic!("{id} has a body"));
        assert_eq!(b.platform, Platform::Ios, "{id}");
    }
    let mut ids: Vec<&str> = all().iter().map(|b| b.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "ids are unique");
    for b in all() {
        assert!(!b.source.note().is_empty() && !b.screens.is_empty(), "{}", b.id);
    }
}

#[test]
fn unknown_types_get_a_generic_body_from_the_screen_they_report() {
    let ty = apple_type("iPhone-18-Pro", "iPhone 18 Pro", "iPhone", (1206, 2622), 3);
    let b = for_apple_type(&ty).expect("generic body");
    assert!(matches!(b.source, Source::Generic(_)));
    assert!(b.shows((1206, 2622)));
    assert!(Arc::ptr_eq(&b, &for_apple_type(&ty).expect("again")), "cached");
    // A known type whose reported screen differs from the row's isn't drawn with the row.
    let odd = apple_type("iPhone-17-Pro", "iPhone 17 Pro", "iPhone", (1179, 2556), 3);
    let b = for_apple_type(&odd).expect("generic body");
    assert!(matches!(b.source, Source::Generic(_)) && b.shows((1179, 2556)));
    let known = apple_type("iPhone-17-Pro", "iPhone 17 Pro", "iPhone", (1206, 2622), 3);
    assert_eq!(for_apple_type(&known).map(|b| b.id.clone()).as_deref(), Some("iPhone-17-Pro"));
    assert!(for_apple_type(&apple_type("Apple-TV-4K-3rd-generation-4K", "Apple TV 4K", "Apple TV", (3840, 2160), 1)).is_none());
    // An AVD on a custom screen, and a square watch.
    let custom = kind(Platform::Android, FormFactor::Phone).profile().with_screen(1080, 2400, Density::Dpi(420));
    let b = for_android("custom", &custom).expect("generic body");
    assert!(b.shows((1080, 2400)) && matches!(b.source, Source::Generic(_)));
    let mut square = kind(Platform::Android, FormFactor::Watch).profile();
    square.shape = Shape::Rectangle;
    assert_eq!(for_android("wearos_square", &square).expect("watch").screens[0].shape, Shape::Rectangle);
}

#[test]
fn screens_match_the_screen_simee_reports() {
    for b in all() {
        for s in &b.screens {
            let [w, h] = s.size();
            let what = format!("{} {:?}", b.id, s.display);
            match s.published {
                Some(Published::Diagonal(inches)) => {
                    assert!(close(s.diagonal_inches(), inches, PUBLISHED), "{what}: {} vs {inches} in", s.diagonal_inches())
                }
                Some(Published::Area(mm2)) => assert!(close(w * h, mm2, PUBLISHED), "{what}: {} vs {mm2} mm²", w * h),
                None => assert!(matches!(b.source, Source::Generic(_)), "{what}: a published body publishes its screen"),
            }
            // On the face, inside the rounded-over edge.
            let [l, r, bottom, top] = s.rect();
            let [bw, bh, _] = b.size;
            let m = b.bevel;
            assert!(l >= -bw / 2.0 + m && r <= bw / 2.0 - m && bottom >= -bh / 2.0 + m && top <= bh / 2.0 - m, "{what}: inside the body");
        }
    }
    // The pixels are simee's own: reference profiles, foldable definitions, the iPhone Duo. The
    // screen drawn on the body has the shape simee reports (its aspect, and at the density a
    // generic body assumes, its size in millimetres).
    for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Foldable, FormFactor::Watch] {
        let p = kind(Platform::Android, f).profile();
        let b = for_profile(&p).expect("body");
        let main = b.screen(Display::Main).or(b.screen(Display::Inner)).expect("screen");
        assert_eq!((main.px, main.ppi, main.shape), ((p.width, p.height), 0.0 + dpi(p.density), p.shape), "{f:?}");
        drawn_as_reported(&b, main.display, (p.width, p.height), Some(dpi(p.density)));
    }
    for d in fold::devices() {
        let b = get(d.id).expect("body");
        assert_eq!(b.screen(Display::Inner).map(|s| s.px), Some((d.lcd.0, d.lcd.1)), "{}", d.id);
        // A published body has its maker's density, not the emulator's.
        drawn_as_reported(&b, Display::Inner, (d.lcd.0, d.lcd.1), None);
        if d.hinge.fold_at == fold::StockPosture::Closed {
            let r = d.hinge.folded.expect("outer");
            assert_eq!(b.screen(Display::Outer).map(|s| (s.px, s.face)), Some(((r.width, r.height), Face::Back)), "{}", d.id);
            drawn_as_reported(&b, Display::Outer, (r.width, r.height), None);
        }
    }
    for (id, f) in [
        ("iPhone-17-Pro", FormFactor::Phone),
        ("iPad-Pro-11-inch-M4", FormFactor::Tablet),
        ("Apple-Watch-Series-10-46mm", FormFactor::Watch),
    ] {
        let p = kind(Platform::Ios, f).profile();
        let b = get(id).expect("row");
        assert!(b.shows((p.width, p.height)), "{id} shows the {f:?} reference screen");
        drawn_as_reported(&b, Display::Main, (p.width, p.height), None);
    }
    let duo = get("iPhone-Duo").expect("duo");
    assert_eq!(duo.screen(Display::Outer).map(|s| s.px), Some((1398, 2034)));
    assert!(duo.shows((2007, 2853)), "the inner LCD, stored portrait");
    drawn_as_reported(&duo, Display::Outer, (1398, 2034), None);
    drawn_as_reported(&duo, Display::Inner, (2007, 2853), None);
}

/// The screen drawn for `display` has the aspect of the pixels simee reports for it, turned if it
/// is stored on its side, and with the density simee reports (`dpi`), the size those pixels make.
fn drawn_as_reported(b: &Body, display: Display, (rw, rh): (u32, u32), dpi: Option<f32>) {
    let s = b.screen(display).unwrap_or_else(|| panic!("{} has a {display:?} screen", b.id));
    let [w, h] = s.size();
    let (rw, rh) = if (w >= h) == (rw >= rh) { (rw as f32, rh as f32) } else { (rh as f32, rw as f32) };
    let what = format!("{} {display:?}", b.id);
    assert!(close(w / h, rw / rh, EXACT), "{what}: drawn {w}x{h} mm for {rw}x{rh} px");
    if let Some(dpi) = dpi {
        assert!(close(w, rw * MM_PER_INCH / dpi, EXACT) && close(h, rh * MM_PER_INCH / dpi, EXACT), "{what}: size at {dpi} dpi");
    }
}

fn dpi(d: Density) -> f32 {
    match d {
        Density::Dpi(v) => v as f32,
        Density::Scale(_) => f32::NAN,
    }
}

// --- mesh ---

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// The volume the parts enclose if, with positions welded, every edge is shared by exactly two
/// triangles that run it in opposite directions (closed and consistently wound).
fn closed_volume(m: &Mesh, parts: &[Part]) -> Result<f32, String> {
    let mut ids: HashMap<[i64; 3], usize> = HashMap::new();
    let mut edges: HashMap<(usize, usize), u32> = HashMap::new();
    let mut volume = 0.0;
    for part in parts {
        for t in m.part_indices(part).chunks(3) {
            let p = [0, 1, 2].map(|i| m.positions[t[i] as usize]);
            let id = p.map(|q| {
                let key = q.map(|c| (f64::from(c) * 1000.0).round() as i64);
                let next = ids.len();
                *ids.entry(key).or_insert(next)
            });
            if id[0] == id[1] || id[1] == id[2] || id[0] == id[2] {
                continue;
            }
            for (a, b) in [(id[0], id[1]), (id[1], id[2]), (id[2], id[0])] {
                *edges.entry((a, b)).or_default() += 1;
            }
            volume += dot(p[0], cross(p[1], p[2])) / 6.0;
        }
    }
    for (&(a, b), &n) in &edges {
        if n != 1 || edges.get(&(b, a)) != Some(&1) {
            return Err(format!("edge {a}-{b} used {n} times, back {:?}", edges.get(&(b, a))));
        }
    }
    Ok(volume)
}

fn bodies_to_mesh() -> Vec<Arc<Body>> {
    let mut out = all().to_vec();
    out.push(for_apple_type(&apple_type("iPhone-18-Pro", "iPhone 18 Pro", "iPhone", (1206, 2622), 3)).expect("generic"));
    let mut square = kind(Platform::Android, FormFactor::Watch).profile();
    square.shape = Shape::Rectangle;
    out.push(for_profile(&square).expect("square watch"));
    out.extend(unlisted_foldables());
    out
}

/// A foldable's `config.ini` hinge keys, with the screen's.
fn hinge_config(ty: u8, sub_type: u8, ranges: &str, area: Option<&str>) -> impl Fn(&str) -> Option<String> {
    let mut map: HashMap<&str, String> = HashMap::new();
    for (k, v) in [
        ("tag.id", "google_apis"),
        ("hw.lcd.width", "1080"),
        ("hw.lcd.height", "2640"),
        ("hw.lcd.density", "420"),
        ("hw.sensor.hinge", "yes"),
        ("hw.sensor.hinge.defaults", "180"),
        ("hw.displayRegion.0.1.xOffset", "0"),
        ("hw.displayRegion.0.1.yOffset", "0"),
        ("hw.displayRegion.0.1.width", "720"),
        ("hw.displayRegion.0.1.height", "748"),
    ] {
        map.insert(k, v.to_owned());
    }
    map.insert("hw.sensor.hinge.type", ty.to_string());
    map.insert("hw.sensor.hinge.sub_type", sub_type.to_string());
    map.insert("hw.sensor.hinge.ranges", ranges.to_owned());
    if let Some(a) = area {
        map.insert("hw.sensor.hinge.areas", a.to_owned());
    }
    move |k| map.get(k).cloned()
}

/// The body of an unlisted foldable whose AVD has this hinge.
fn avd_foldable(name: &str, get: impl Fn(&str) -> Option<String>) -> Arc<Body> {
    let profile = crate::form_factor::android_profile(&get);
    assert_eq!(profile.kind.form_factor, FormFactor::Foldable);
    for_android_hinge(name, &profile, fold::Hinge::from_config(&get).as_ref()).expect("body")
}

/// Unlisted foldables as AVDs describe them: a flip, a book over a shorter range, and a dual-screen
/// device with a physical hinge between its screens.
fn unlisted_foldables() -> Vec<Arc<Body>> {
    vec![
        avd_foldable("flip", hinge_config(0, 0, "0-180", None)),
        avd_foldable("shallow_book", hinge_config(1, 0, "0-120", Some("540-0-0-2640"))),
        avd_foldable("two_screens", hinge_config(1, 1, "0-360", Some("1080-0-84-1800"))),
    ]
}

#[test]
fn meshes_are_well_formed() {
    for b in bodies_to_mesh() {
        let m = mesh(&b);
        let id = &b.id;
        let n = m.positions.len();
        assert!(n > 0 && m.normals.len() == n && m.uvs.len() == n, "{id}");
        assert_eq!(m.indices.len() % 3, 0, "{id}");
        assert!(m.indices.iter().all(|&i| (i as usize) < n), "{id}: indices in range");
        assert_eq!(m.parts.iter().map(|p| p.count as usize).sum::<usize>(), m.indices.len(), "{id}: parts cover the indices");
        assert!(m.normals.iter().all(|v| (dot(*v, *v).sqrt() - 1.0).abs() < 1e-4), "{id}: unit normals");
        assert!(m.uvs.iter().all(|uv| uv.iter().all(|c| (0.0..=1.0).contains(c))), "{id}: uvs in 0..1");
        let panels = b.panels().len();
        for panel in 0..panels {
            // The shell and the screens set into it close up, and face out.
            let outer: Vec<Part> =
                m.parts.iter().filter(|p| p.panel == panel && matches!(p.role, Role::Shell | Role::Screen(_))).copied().collect();
            assert_eq!(outer.iter().filter(|p| p.role == Role::Shell).count(), 1, "{id}: one shell per panel");
            let v = closed_volume(&m, &outer).unwrap_or_else(|e| panic!("{id} panel {panel}: {e}"));
            let p = b.panels()[panel];
            let slab = p.size[0] * p.size[1] * b.size[2];
            assert!(v > 0.6 * slab && v <= slab * 1.001, "{id} panel {panel}: volume {v} of {slab}");
        }
        for part in m.parts.iter().filter(|p| matches!(p.role, Role::Bump | Role::Button | Role::Crown)) {
            let v = closed_volume(&m, &[*part]).unwrap_or_else(|e| panic!("{id} {:?}: {e}", part.role));
            assert!(v > 0.0, "{id} {:?} faces out", part.role);
        }
        // Each triangle winds the way its normals face.
        for part in &m.parts {
            for t in m.part_indices(part).chunks(3) {
                let [a, b2, c] = [0, 1, 2].map(|i| t[i] as usize);
                let g = cross(sub(m.positions[b2], m.positions[a]), sub(m.positions[c], m.positions[a]));
                if dot(g, g) < 1e-12 {
                    continue;
                }
                let avg = [0, 1, 2].map(|k| m.normals[a][k] + m.normals[b2][k] + m.normals[c][k]);
                assert!(dot(g, avg) > 0.0, "{id} {:?}: a triangle faces in", part.role);
            }
        }
        // Each screen's UVs span 0..1 (across both panels of a foldable's inner screen), with v
        // growing down the screen, and its face covers the screen's rect.
        for s in &b.screens {
            let verts: Vec<usize> = m
                .parts
                .iter()
                .filter(|p| p.role == Role::Screen(s.display))
                .flat_map(|p| m.part_indices(p).iter().map(|&i| i as usize))
                .collect();
            assert!(!verts.is_empty(), "{id} {:?} is drawn", s.display);
            let span = |f: &dyn Fn(usize) -> f32| verts.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &i| (lo.min(f(i)), hi.max(f(i))));
            for k in 0..2 {
                let (lo, hi) = span(&|i| m.uvs[i][k]);
                assert!(lo.abs() < 1e-4 && (hi - 1.0).abs() < 1e-4, "{id} {:?}: uv {k} spans {lo}..{hi}", s.display);
            }
            let top = verts.iter().copied().max_by(|&i, &j| m.positions[i][1].total_cmp(&m.positions[j][1])).expect("top");
            assert!(m.uvs[top][1] < 1e-4, "{id} {:?}: v is 0 at the top", s.display);
            let [l, r, bottom, t] = s.rect();
            let (x0, x1) = span(&|i| m.positions[i][0]);
            let (y0, y1) = span(&|i| m.positions[i][1]);
            assert!([x0 - l, x1 - r, y0 - bottom, y1 - t].iter().all(|d| d.abs() < 1e-3), "{id} {:?}: face is the rect", s.display);
            // A foldable's inner screen is two halves, one per panel, that meet at the hinge.
            if b.fold.is_some() && s.display == Display::Inner {
                halves_meet(&b, &m);
            }
            let z = m.positions[verts[0]][2];
            let face = if s.face == Face::Front { b.size[2] / 2.0 } else { -b.size[2] / 2.0 };
            assert!((z - face).abs() < 1e-4, "{id} {:?}: on its face", s.display);
        }
    }
}

#[test]
fn meshes_are_cached_per_device_type() {
    let b = get("iPhone-17-Pro").expect("row");
    let first = cached_mesh(&b);
    assert!(Arc::ptr_eq(&first, &cached_mesh(&b)), "the same mesh");
    assert!(Arc::ptr_eq(&b, &get("iPhone-17-Pro").expect("row")), "the same body");
    assert_eq!(*first, mesh(&b));
    let other = cached_mesh(&get("iPhone-17").expect("row"));
    assert!(!Arc::ptr_eq(&first, &other));
}

fn apply(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|i| m[0][i] * p[0] + m[1][i] * p[1] + m[2][i] * p[2] + m[3][i])
}

/// The vertices of a foldable's inner screen on each panel.
fn inner_halves(m: &Mesh) -> [Vec<[f32; 3]>; 2] {
    [0, 1].map(|panel| {
        m.parts
            .iter()
            .filter(|p| p.panel == panel && p.role == Role::Screen(Display::Inner))
            .flat_map(|p| m.part_indices(p).iter().map(|&i| m.positions[i as usize]))
            .collect()
    })
}

/// A foldable's inner screen halves reach the hinge: both stop half the body's screen gap short
/// of it (none for one flexible display, where they meet exactly), spanning the same stretch of
/// it. A flexible display's two edges are then the same line at every angle it folds in to, the
/// halves bending at the hinge.
fn halves_meet(b: &Body, m: &Mesh) {
    let id = &b.id;
    let f = b.fold.expect("a fold");
    let (a, o) = if f.hinge.vertical { (0, 1) } else { (1, 0) };
    let [h0, h1] = inner_halves(m);
    assert!(!h0.is_empty() && !h1.is_empty(), "{id}: a half on each panel");
    let near0 = h0.iter().map(|p| p[a]).fold(f32::MIN, f32::max);
    let near1 = h1.iter().map(|p| p[a]).fold(f32::MAX, f32::min);
    let half_gap = f.screen_gap / 2.0;
    assert!((near0 - (f.hinge.at - half_gap)).abs() < 1e-4, "{id}: panel 0's half ends at {near0}");
    assert!((near1 - (f.hinge.at + half_gap)).abs() < 1e-4, "{id}: panel 1's half starts at {near1}");
    let edge = |half: &[[f32; 3]], x: f32| half.iter().filter(|p| (p[a] - x).abs() < 1e-4).copied().collect::<Vec<_>>();
    let (e0, e1) = (edge(&h0, near0), edge(&h1, near1));
    let reach = |e: &[[f32; 3]]| e.iter().map(|p| p[o]).fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let ((lo0, hi0), (lo1, hi1)) = (reach(&e0), reach(&e1));
    assert!((lo0 - lo1).abs() < 1e-4 && (hi0 - hi1).abs() < 1e-4, "{id}: the edges span {lo0}..{hi0} and {lo1}..{hi1}");
    if f.screen_gap > 0.0 {
        return;
    }
    let (lo, hi) = f.hinge.range;
    for angle in [lo, (lo + HingeAxis::FLAT.min(hi)) / 2.0, HingeAxis::FLAT.min(hi)] {
        let turn = f.hinge.transform(angle);
        let moved: Vec<[f32; 3]> = e1.iter().map(|&p| apply(&turn, p)).collect();
        let same = |p: &[f32; 3], q: &[f32; 3]| (0..3).all(|k| (p[k] - q[k]).abs() < 1e-3);
        assert!(moved.iter().all(|p| e0.iter().any(|q| same(p, q))), "{id} at {angle}°: panel 1's edge leaves panel 0's");
        assert!(e0.iter().all(|q| moved.iter().any(|p| same(p, q))), "{id} at {angle}°: panel 0's edge leaves panel 1's");
    }
}

/// A foldable has its two panels either side of the hinge, in the order the transform folds them,
/// a screen half on each, and a hinge that closes and opens through `range`.
fn check_fold(b: &Body, vertical: bool, range: (f32, f32)) {
    let id = &b.id;
    let f = b.fold.expect("a fold");
    assert!(f.hinge.vertical == vertical && f.hinge.range == range, "{id}: {:?}", f.hinge);
    let a = usize::from(!vertical);
    let [p0, p1] = f.panels;
    let t = b.size[2];
    assert!(
        close(p0.size[a] + p1.size[a], b.size[a], EXACT) && close(p0.size[1 - a], b.size[1 - a], EXACT),
        "{id}: the panels make the body"
    );
    assert!(p0.center[a] < f.hinge.at && p1.center[a] > f.hinge.at, "{id}: one either side");
    assert_eq!(f.hinge_side(0), if vertical { Side::Right } else { Side::Top }, "{id}");
    assert_eq!(f.hinge_side(1), if vertical { Side::Left } else { Side::Bottom }, "{id}");
    let m = mesh(b);
    for panel in [0, 1] {
        assert!(m.parts.iter().any(|p| p.panel == panel && p.role == Role::Screen(Display::Inner)), "{id}: inner screen on {panel}");
    }
    // A point of panel 1 at `z`, on the line through its centre across the hinge.
    let on_axis = |z: f32| {
        let mut p = [0.0, 0.0, z];
        p[a] = p1.center[a];
        p
    };
    // Flat is where the mesh is authored (when the hinge opens that far); closed folds panel 1
    // onto panel 0, screen to screen.
    if range.1 >= HingeAxis::FLAT {
        let flat = f.hinge.transform(HingeAxis::FLAT);
        let ident = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
        assert!(flat.iter().flatten().zip(ident.iter().flatten()).all(|(a, b)| (a - b).abs() < 1e-6), "{id}: flat");
    }
    let closed = f.hinge.transform(range.0);
    let q = apply(&closed, on_axis(t / 2.0));
    assert!((q[a] - (2.0 * f.hinge.at - p1.center[a])).abs() < 1e-3 && (q[2] - t / 2.0).abs() < 1e-3, "{id}: closed {q:?}");
    let back = apply(&closed, on_axis(-t / 2.0));
    assert!((back[2] - 1.5 * t).abs() < 1e-3, "{id}: panel 1's back is on top {back:?}");
    if range.1 > 180.0 {
        // Folded all the way back, the backs meet.
        let q = apply(&f.hinge.transform(range.1), on_axis(-t / 2.0));
        assert!((q[a] - (2.0 * f.hinge.at - p1.center[a])).abs() < 1e-3 && (q[2] + t / 2.0).abs() < 1e-3, "{id}: flipped {q:?}");
    }
    // Half open, panel 1 stands up off the front.
    let up = apply(&f.hinge.transform(90.0f32.clamp(range.0, range.1)), on_axis(t / 2.0));
    assert!(up[2] > t && (up[a] - f.hinge.at).abs() < 1e-3, "{id}: half open {up:?}");
}

#[test]
fn foldables_have_two_panels_and_a_hinge() {
    for (id, range) in
        [("pixel_fold", fold::pixel_fold().hinge.range), ("surface_duo", fold::dual_screen().hinge.range), ("iPhone-Duo", (0.0, 180.0))]
    {
        check_fold(&get(id).expect("foldable"), true, range);
    }
    let pf = get("pixel_fold").expect("pixel fold");
    let outer = pf.screen(Display::Outer).expect("outer screen");
    assert_eq!((outer.face, pf.panel_at(outer.center)), (Face::Back, 0), "the cover screen is on the back of the left panel");
    assert!(get("iPhone-17").expect("phone").fold.is_none());
}

#[test]
fn a_flexible_inner_screen_is_one_face_across_the_hinge() {
    // One flexible display: the halves meet at the hinge, with no strip of body between them, and
    // each is on its own panel so the face bends there.
    for id in ["pixel_fold", "iPhone-Duo"] {
        let b = get(id).expect("foldable");
        let f = b.fold.expect("a fold");
        assert_eq!(f.screen_gap, 0.0, "{id}");
        let m = mesh(&b);
        halves_meet(&b, &m);
        let [h0, h1] = inner_halves(&m);
        let (right0, left1) = (h0.iter().map(|p| p[0]).fold(f32::MIN, f32::max), h1.iter().map(|p| p[0]).fold(f32::MAX, f32::min));
        assert!(right0 == left1 && right0 == f.hinge.at, "{id}: {right0} meets {left1} at {}", f.hinge.at);
        // The face is as wide as the screen, across the hinge.
        let s = b.screen(Display::Inner).expect("inner");
        assert!(close(
            h1.iter().map(|p| p[0]).fold(f32::MIN, f32::max) - h0.iter().map(|p| p[0]).fold(f32::MAX, f32::min),
            s.size()[0],
            EXACT
        ));
    }
    // Two real screens keep the hinge's width between them: the Surface Duo's 84 px at 401 ppi.
    let duo = get("surface_duo").expect("dual screen");
    let f = duo.fold.expect("a fold");
    assert!(close(f.screen_gap, 84.0 * MM_PER_INCH / 401.0, EXACT), "{}", f.screen_gap);
    let m = mesh(&duo);
    halves_meet(&duo, &m);
    let [h0, h1] = inner_halves(&m);
    let gap = h1.iter().map(|p| p[0]).fold(f32::MAX, f32::min) - h0.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
    assert!(close(gap, f.screen_gap, EXACT), "two screens {gap} mm apart");
}

#[test]
fn an_unlisted_foldable_takes_its_avds_hinge() {
    // A flip: the fold runs across the screen, the lid on top.
    let flip = avd_foldable("flip", hinge_config(0, 0, "0-180", None));
    check_fold(&flip, false, (0.0, 180.0));
    let f = flip.fold.expect("a fold");
    assert_eq!(f.screen_gap, 0.0, "a fold in one screen");
    assert!(f.panels[0].center[1] < 0.0 && f.panels[1].center[1] > 0.0, "the lid is panel 1, on top");
    assert!(close(f.panels[0].size[0], flip.size[0], EXACT) && close(f.panels[0].size[1] * 2.0, flip.size[1], EXACT));
    let s = flip.screen(Display::Inner).expect("inner");
    assert_eq!(s.px, (1080, 2640));
    assert!(flip.size[1] > flip.size[0], "a flip's body is as tall as its screen");
    let outer = flip.screen(Display::Outer).expect("cover screen");
    assert_eq!((outer.px, outer.face, flip.panel_at(outer.center)), ((720, 748), Face::Back, 1), "on the back of the lid");
    assert!(flip.bump.is_some_and(|b| flip.panel_at(b.center) == 1), "the camera is on the lid too");
    halves_meet(&flip, &mesh(&flip));
    // The hinge's own range, and a vertical hinge over it, not the default 0-180° book.
    let shallow = avd_foldable("shallow_book", hinge_config(1, 0, "0-120", Some("540-0-0-2640")));
    check_fold(&shallow, true, (0.0, 120.0));
    // Two screens with a physical hinge between them, the gap the hinge's width at the AVD's density.
    let duo = avd_foldable("two_screens", hinge_config(1, 1, "0-360", Some("1080-0-84-1800")));
    check_fold(&duo, true, (0.0, 360.0));
    assert!(close(duo.fold.expect("a fold").screen_gap, 84.0 * MM_PER_INCH / 420.0, EXACT));
    halves_meet(&duo, &mesh(&duo));
    // The bodies are told apart by their hinge, and cached by it.
    let again = avd_foldable("flip", hinge_config(0, 0, "0-180", None));
    assert!(Arc::ptr_eq(&flip, &again) && !Arc::ptr_eq(&flip, &shallow));
    // Without a hinge description the body is the vertical 0-180° book it always was; so is a
    // profile that is not a foldable, whatever hinge it is handed.
    let get = hinge_config(0, 0, "0-180", None);
    let profile = crate::form_factor::android_profile(&get);
    let book = for_android("flip", &profile).expect("body");
    check_fold(&book, true, (0.0, 180.0));
    assert_eq!(for_profile(&profile).map(|b| b.id.clone()), Some(book.id.clone()));
    let hinge = fold::Hinge::from_config(&get);
    let phone = kind(Platform::Android, FormFactor::Phone).profile();
    assert_eq!(for_profile_hinge(&phone, hinge.as_ref()).map(|b| b.id.clone()), for_profile(&phone).map(|b| b.id.clone()));
    // A hinge range that can't be a hinge's falls back to the default.
    let mut odd = fold::pixel_fold().hinge;
    odd.range = (200.0, 100.0);
    let body = for_profile_hinge(&foldable_profile(&fold::pixel_fold()), Some(&odd)).expect("body");
    assert_eq!(body.fold.map(|f| f.hinge.range), Some((0.0, 180.0)));
}

#[test]
fn the_iphone_duo_inner_screen_lies_on_its_side() {
    // The face is landscape (2853x2007) and its UVs are upright: (0, 0) at its top-left, u to the
    // right, v down. The frame arrives 2007x2853 and goes on a quarter turn clockwise.
    let b = get("iPhone-Duo").expect("duo");
    let s = b.screen(Display::Inner).expect("inner");
    assert_eq!(s.px, (2853, 2007));
    let [w, h] = s.size();
    assert!(close(w / h, 2853.0 / 2007.0, EXACT));
    let m = mesh(&b);
    let verts: Vec<usize> = m
        .parts
        .iter()
        .filter(|p| p.role == Role::Screen(Display::Inner))
        .flat_map(|p| m.part_indices(p).iter().map(|&i| i as usize))
        .collect();
    let [l, r, bottom, top] = s.rect();
    for &i in &verts {
        let [x, y, _] = m.positions[i];
        let want = [(x - l) / (r - l), (top - y) / (top - bottom)];
        assert!((m.uvs[i][0] - want[0]).abs() < 1e-4 && (m.uvs[i][1] - want[1]).abs() < 1e-4, "uv {:?} at ({x}, {y})", m.uvs[i]);
    }
    // The face's own corners, by extreme vertices.
    let extreme = |key: &dyn Fn(usize) -> f32| verts.iter().copied().max_by(|&i, &j| key(i).total_cmp(&key(j))).expect("vertex");
    let (left, right) = (extreme(&|i| -m.positions[i][0]), extreme(&|i| m.positions[i][0]));
    let (upper, lower) = (extreme(&|i| m.positions[i][1]), extreme(&|i| -m.positions[i][1]));
    assert!(m.uvs[left][0] < 1e-4 && (m.uvs[right][0] - 1.0).abs() < 1e-4, "u runs left to right");
    assert!(m.uvs[upper][1] < 1e-4 && (m.uvs[lower][1] - 1.0).abs() < 1e-4, "v runs top to bottom");
    // The frame's (u, v) on the face, a quarter turn clockwise: its top-left corner lands on the
    // face's top-right, its top-right on the bottom-right, its bottom-left on the top-left.
    let onto_face = |[u, v]: [f32; 2]| [1.0 - v, u];
    let into_frame = |[u, v]: [f32; 2]| [v, 1.0 - u];
    assert_eq!(onto_face([0.0, 0.0]), [1.0, 0.0]);
    assert_eq!(onto_face([1.0, 0.0]), [1.0, 1.0]);
    assert_eq!(onto_face([0.0, 1.0]), [0.0, 0.0]);
    for uv in [[0.0, 0.0], [0.3, 0.8], [1.0, 1.0], [0.5, 0.25]] {
        assert_eq!(onto_face(into_frame(uv)), uv, "the two maps are inverses");
    }
    // The frame's right edge is the face's bottom: `swipe --edge right` goes home from there.
    assert_eq!(onto_face([1.0, 0.5])[1], 1.0);
    // Its 2007x2853 pixels fill the 2853x2007 face turned: the frame's aspect is the face's flipped.
    assert!(close(2007.0 / 2853.0, h / w, EXACT));
}
