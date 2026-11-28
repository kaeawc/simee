//! The landing page, set as a paper: the title block with Figure 1 (the turning gem), the
//! abstract, the headline measurements (one table, iOS and Android), the quick start and where
//! to read on. Every number is the README's own; the page adds captions only.

use rune::{Cx, El, col, el, memo, row, text};

use crate::content::{self, REPO};
use crate::gem;
use crate::tokens::*;
use crate::ui::{MdCx, caption, centered, link_text, markdown, prose, section};
use crate::{State, angle, md_cx};

/// The line under the title (owner's words, 2026-11-27).
pub const SUBTITLE: &str = "Rust iOS sim and Android emu - Fast & Feature Rich";

/// The README sections the page quotes.
pub const README_SECTIONS: [&str; 2] = ["Performance", "Quick start"];

pub fn page(s: &State, cx: &Cx<State>, bp: Bp) -> El {
    let width = cx.layout_size("main").map_or(MEASURE, |sz| sz[0]).min(MEASURE);
    // The quoted sections have no images (a test checks): the memo rebuilds for the column's
    // width, which sizes Table 1, not for a picture arriving.
    let mc = md_cx(s, cx, bp);
    let version = (bp, dark(), mc.column.to_bits());
    col().key("home").w_full().child(title_block(angle(s, cx), width, bp)).child(memo("body", version, || body(&mc, bp)))
}

fn title_block(angle: f32, width: f32, bp: Bp) -> El {
    col()
        .key("title-block")
        .w_full()
        .items_center()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(centered("title", "simee", bp.display(), fg()).role("heading").level(1))
        .child(centered("subtitle", SUBTITLE, bp.title(), fg2()).mt(4.0))
        .child(link_text("byline", "github.com/kaeawc/simee", REPO, SMALL).mt(10.0))
        .child(
            col()
                .key("fig1")
                .role("figure")
                .items_center()
                .mt(20.0)
                .child(gem::figure(angle, width))
                .child(centered("fig1-cap", "Figure 1. The simee mark, drawn and turned by this page's Rune app.", SMALL, fg2()).mt(4.0)),
        )
}

fn body(mc: &MdCx, bp: Bp) -> El {
    let performance = strip_heading(&content::readme_section(README_SECTIONS[0]));
    let quick = content::without_tables(&strip_heading(&content::readme_section(README_SECTIONS[1])));
    col()
        .key("paper")
        .w_full()
        .child(abstract_(bp))
        .child(
            section("performance", "1", "Performance", bp)
                .child(caption(
                    "t1-cap",
                    "Table 1",
                    "simee against the stock tools, a measure a row: Android against the SDK emulator (QEMU), iOS against Simulator.app.",
                ))
                .child(markdown("t1-md", &performance, mc)),
        )
        .child(section("quick-start", "2", "Quick start", bp).child(markdown("qs-md", &quick, mc)))
        .child(further(bp))
}

fn strip_heading(md: &str) -> String {
    md.split_once('\n').map_or(String::new(), |(_, rest)| rest.to_owned())
}

fn abstract_(bp: Bp) -> El {
    let inset = if bp.phone() { 0.0 } else { 36.0 };
    col()
        .key("abstract")
        .role("region")
        .label("Abstract")
        .w_full()
        .max_w(MEASURE)
        .self_center()
        .px(inset)
        .pt(28.0)
        .child(centered("abstract-h", "Abstract", BOLD, fg()).mb(6.0))
        .child(prose("abstract-p", &content::readme_lede_code(), BODY, fg()))
}

/// Where to read on, numbered as a paper's references.
fn further(bp: Bp) -> El {
    let refs: [(&str, &str); 6] = [
        ("Benchmarks: every comparison, its host and how to rerun it", "/benchmarks/"),
        ("Debugging: what simee shows of a device that the stock tools don't", "/debugging/"),
        ("Limitations: what doesn't work yet", "/limitations/"),
        ("Device concepts: every form factor on both platforms", "/devices/"),
        ("Documentation", "/docs/"),
        ("Source code (MIT)", REPO),
    ];
    let mut list = col().key("refs-list").role("list").gap(4.0);
    for (i, (label, href)) in refs.iter().enumerate() {
        list = list.child(
            el().key(format!("ref-{i}")).role("listitem").w_full().child(
                row()
                    .key(format!("ref-{i}-row"))
                    .gap(10.0)
                    .items_start()
                    .child(text(format!("[{}]", i + 1)).font(BODY).color(fg2()).w(30.0))
                    .child(link_text(&format!("ref-{i}-a"), label, href, BODY).w(0.0).grow(1.0)),
            ),
        );
    }
    section("further", "", "Further reading", bp).child(list)
}
