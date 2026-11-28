//! Device concepts: every form factor on both platforms, from `sim_core::form_factor` itself:
//! the support matrix with its reasons, each kind's reference profile and inputs. The outlines
//! are drawn to each profile's aspect and shape.

use rune::{El, LineCap, Shape as Drawing, canvas, col, el, row, text};
use sim_core::form_factor::{Density, FormFactor, InputMethod, Kind, Profile, Shape, matrix};
use sim_core::platform::Platform;

use crate::content::REPO;
use crate::tokens::*;
use crate::ui::{link_text, prose, section};

pub fn page(bp: Bp) -> El {
    let mut page = col()
        .key("devices")
        .w_full()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(text("Device concepts").key("dev-h1").role("heading").level(1).font(bp.title()).color(fg()).wrap())
        .child(
            prose(
                "dev-lede",
                "A device is a platform and a form factor, with a reference profile and its inputs. Generated from `sim-core`.",
                BODY,
                fg(),
            )
            .mt(12.0),
        );
    let rows = matrix();
    for (n, ff) in FormFactor::ALL.into_iter().enumerate() {
        let mut cells = if bp.phone() { col() } else { row().items_start() }.key(format!("{}-row", ff.as_str())).w_full().gap(24.0);
        for platform in [Platform::Ios, Platform::Android] {
            let Some((_, _, kind)) = rows.iter().find(|(f, p, _)| *f == ff && *p == platform) else { continue };
            let cell = match kind {
                Ok(kind) => supported(kind),
                Err(why) => refused(ff, platform, why.reason),
            };
            cells = cells.child(if bp.phone() { cell.w_full() } else { cell.w(0.0).grow(1.0) });
        }
        page = page.child(section(ff.as_str(), &(n + 1).to_string(), ff.label(), bp).child(cells));
    }
    page.child(
        row()
            .key("dev-more")
            .gap(18.0)
            .mt(32.0)
            .flex_wrap()
            .child(link_text("dev-doc", "Form factors in depth", "/docs/form-factors/", BODY))
            .child(link_text("dev-src", "form_factor.rs", &format!("{REPO}/blob/main/crates/sim-core/src/form_factor.rs"), BODY)),
    )
}

fn platform_label(p: Platform) -> &'static str {
    match p {
        Platform::Ios => "Apple",
        Platform::Android => "Android",
    }
}

fn supported(kind: &Kind) -> El {
    let profile = kind.profile();
    let key = format!("{}-{}", kind.form_factor.as_str(), kind.platform.as_str());
    let mut facts = col()
        .key(format!("{key}-facts"))
        .gap(2.0)
        .w(0.0)
        .grow(1.0)
        .child(text(kind.label()).key(format!("{key}-h")).role("heading").level(3).font(BOLD).color(fg()).wrap().mb(2.0))
        .child(fact(&key, "screen", &format!("{} × {} px, {}", profile.width, profile.height, density(profile.density))))
        .child(fact(&key, "shape", shape(profile.shape)))
        .child(fact(&key, "inputs", &inputs(&profile.inputs, kind.platform)));
    if let Some(fold) = profile.fold {
        facts = facts.child(fact(&key, "folded", &format!("{} × {} px outer screen", fold.folded_width, fold.folded_height)));
    }
    row().key(key.clone()).role("article").gap(16.0).items_start().child(silhouette(&key, &profile)).child(facts)
}

fn refused(ff: FormFactor, platform: Platform, reason: &str) -> El {
    let key = format!("{}-{}", ff.as_str(), platform.as_str());
    col()
        .key(key.clone())
        .role("article")
        .gap(2.0)
        .child(
            text(format!("No {} {}", platform_label(platform), ff.label().to_lowercase()))
                .key(format!("{key}-h"))
                .role("heading")
                .level(3)
                .font(BOLD)
                .color(fg())
                .wrap(),
        )
        .child(text(capitalize(reason)).key(format!("{key}-why")).font(BODY.italic()).color(fg2()).wrap())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    let out: String = c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect());
    if out.ends_with('.') { out } else { format!("{out}.") }
}

fn fact(key: &str, name: &str, value: &str) -> El {
    row()
        .key(format!("{key}-{name}"))
        .gap(10.0)
        .w_full()
        .child(text(name.to_owned()).font(SMALL.italic()).color(fg2()).w(52.0))
        .child(text(value.to_owned()).font(SMALL).color(fg()).wrap().w(0.0).grow(1.0))
}

fn density(d: Density) -> String {
    match d {
        Density::Dpi(dpi) => format!("{dpi} dpi"),
        Density::Scale(s) => format!("@{s}x"),
    }
}

fn shape(s: Shape) -> &'static str {
    match s {
        Shape::Rectangle => "rectangle",
        Shape::Rounded => "rounded corners",
        Shape::Round => "round",
    }
}

fn inputs(list: &[InputMethod], platform: Platform) -> String {
    list.iter().map(|i| i.label(platform)).collect::<Vec<_>>().join(", ")
}

/// The outline box, in points.
const BOX: f32 = 56.0;

/// What the device looks like, in a 56-point box: a car for automotive, a Vision Pro or glasses
/// for vision, else the screen at its natural orientation (its aspect from the profile's pixels,
/// corners from its shape, a hinge line on a foldable).
fn silhouette(key: &str, p: &Profile) -> El {
    let drawn = match (p.kind.platform, p.kind.form_factor) {
        (_, FormFactor::Auto) => Some(CAR),
        (Platform::Ios, FormFactor::Vision) => Some(VISION_PRO),
        (Platform::Android, FormFactor::Vision) => Some(GLASSES),
        _ => None,
    };
    let frame = col().key(format!("{key}-sil")).size(BOX, BOX).items_center().justify_center().a11y_hidden();
    if let Some(d) = drawn {
        let shape = Drawing::new().view_box(BOX, BOX).stroke_svg(d, 1.25, LineCap::Round, fg());
        return frame.child(canvas(shape).key(format!("{key}-drawn")).size(BOX, BOX));
    }
    frame.child(screen_outline(key, p))
}

/// A car from the side: body, windows and wheels.
const CAR: &str = "M4 36 L4 29 Q4 27 6 26.5 L13 25 L19 18.5 Q20.5 17 23 17 L34 17 Q36 17 37.5 18.5 L43 24.5 L50 26 \
                   Q52.5 26.8 52.5 29 L52.5 36 L46 36 A5 5 0 0 0 36 36 L20 36 A5 5 0 0 0 10 36 Z \
                   M21 24.5 L24 19.5 L29 19.5 L29 24.5 Z M31 19.5 L35 19.5 L39.5 24.5 L31 24.5 Z \
                   M37.5 36 A3.5 3.5 0 1 0 44.5 36 A3.5 3.5 0 1 0 37.5 36 Z \
                   M11.5 36 A3.5 3.5 0 1 0 18.5 36 A3.5 3.5 0 1 0 11.5 36 Z";

/// Apple Vision Pro from the front: the curved glass over the nose bridge, and the band's ends.
const VISION_PRO: &str = "M8 22 Q28 14 48 22 Q51 24 50.5 30 Q50 36 44 37.5 Q38 38.5 34 35 Q31 32 28 32 Q25 32 22 35 \
                          Q18 38.5 12 37.5 Q6 36 5.5 30 Q5 24 8 22 Z M10 24.5 Q28 18 46 24.5 \
                          M5.5 27 L1 29 M50.5 27 L55 29";

/// Glasses from the front: two lenses, the bridge and the temples.
const GLASSES: &str = "M8 21 L21 21 Q25 21 25 25 L25 31 Q25 35 21 35 L8 35 Q4 35 4 31 L4 25 Q4 21 8 21 Z \
                       M35 21 L48 21 Q52 21 52 25 L52 31 Q52 35 48 35 L35 35 Q31 35 31 31 L31 25 Q31 21 35 21 Z \
                       M25 25 Q28 22 31 25 M4 24 L1 21 M52 24 L55 21";

fn screen_outline(key: &str, p: &Profile) -> El {
    let (w, h) = (p.width.max(1) as f32, p.height.max(1) as f32);
    let scale = BOX / w.max(h);
    let (sw, sh) = ((w * scale).max(10.0), (h * scale).max(10.0));
    let radius = match p.shape {
        Shape::Round => sw.min(sh) / 2.0,
        Shape::Rounded => (sw.min(sh) * 0.16).max(3.0),
        Shape::Rectangle => 1.5,
    };
    let mut screen = el().key(format!("{key}-screen")).size(sw, sh).radius(radius).border(1.25, fg());
    if p.fold.is_some() {
        screen = screen.items_center().justify_center().child(el().key(format!("{key}-hinge")).w(1.0).h(sh - 2.5).fill(fg2()));
    }
    screen
}
