//! The pages under the landing page's headline: the full comparison (Benchmarks), the debug
//! surface (Debugging) and what doesn't work yet (Limitations). Each quotes the README or a doc;
//! the page adds captions and links to the doc behind it.

use rune::{El, col, text};

use crate::content;
use crate::tokens::*;
use crate::ui::{MdCx, caption, link_text, markdown, prose};

fn page(key: &str, title: &str, bp: Bp) -> El {
    col()
        .key(key.to_owned())
        .w_full()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(text(title.to_owned()).key(format!("{key}-h1")).role("heading").level(1).font(bp.title()).color(fg()).wrap())
}

/// The README's comparison with the stock tools, a captioned table per platform.
pub fn benchmarks(mc: &MdCx, bp: Bp) -> El {
    let compared = content::subsections(&content::readme_section("Compared with the stock tools"));
    let mut out = page("benchmarks", "Benchmarks", bp).child(
        prose("bench-p", "Each table compares simee with the stock tool on the same host and day. Every number, its host, date and the command that reruns it are in the benchmarks doc.", BODY, fg())
            .max_w(MEASURE)
            .mt(12.0),
    );
    for (i, (heading, body)) in compared.iter().enumerate() {
        let n = i + 1;
        out = out.child(caption(&format!("bt{n}-cap"), &format!("Table {n}"), &heading.replace('`', "")).mt(28.0)).child(markdown(
            &format!("bt{n}-md"),
            &content::until(body, "How each number"),
            mc,
        ));
    }
    out.child(
        link_text("bench-doc", "Every benchmark, how it was measured and what isn't measured yet", "/docs/benchmarks/", BODY).mt(28.0),
    )
}

/// docs/debugging.md's iOS debug surface: the table, how to reach it, and Android's paragraph.
pub fn debugging(mc: &MdCx, bp: Bp) -> El {
    let debug = content::doc_section("debugging", "iOS debug surface");
    page("debugging", "Debugging", bp)
        .child(caption("dt1-cap", "Table 1", "The iOS debug surface, through the CLI, the desktop panel and the socket API.").mt(20.0))
        .child(markdown("dt1-md", &debug, mc))
        .child(link_text("debug-doc", "Network shaping, audio and Android's counters", "/docs/debugging/", BODY).mt(28.0))
}

/// The README's "Not yet".
pub fn limitations(mc: &MdCx, bp: Bp) -> El {
    let not_yet = content::readme_section("Not yet");
    let body = not_yet.split_once('\n').map_or("", |(_, rest)| rest);
    page("limitations", "Limitations", bp)
        .child(col().key("lim-body").w_full().mt(16.0).child(markdown("lim-md", body, mc)))
        .child(link_text("lim-roadmap", "The Android roadmap", "/docs/roadmap-android/", BODY).mt(20.0))
}
