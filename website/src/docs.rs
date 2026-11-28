//! The docs: `/docs/` lists every published doc by group, and `/docs/{slug}/` sets one as a
//! paper section with its contents above it.

use rune::{El, col, row, text};

use crate::content::{DOCS, Doc, GROUPS, REPO, outline};
use crate::tokens::*;
use crate::ui::{MdCx, link_text, markdown, section};

pub fn index(bp: Bp) -> El {
    let mut page = col()
        .key("docs")
        .w_full()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(text("Documentation").key("docs-h1").role("heading").level(1).font(bp.title()).color(fg()).wrap());
    for (n, group) in GROUPS.iter().enumerate() {
        let mut list = col().key(format!("g-{group}-list")).role("list").w_full().gap(12.0);
        for doc in DOCS.iter().filter(|d| d.group == *group) {
            list = list.child(
                col()
                    .key(format!("d-{}", doc.slug))
                    .role("listitem")
                    .w_full()
                    .gap(2.0)
                    .child(link_text(&format!("d-{}-a", doc.slug), doc.title(), &doc.href(), BOLD))
                    .child(text(first_sentence(&doc.summary())).font(SMALL).color(fg2()).wrap()),
            );
        }
        page = page.child(section(&format!("g-{group}"), &(n + 1).to_string(), group, bp).child(list));
    }
    page
}

/// The summary's first sentence: a one-line description under the doc's title.
fn first_sentence(s: &str) -> String {
    match s.find(". ") {
        Some(at) => s[..=at].to_owned(),
        None => s.to_owned(),
    }
}

pub fn page(doc: &Doc, mc: &MdCx, bp: Bp) -> El {
    let body = doc.body();
    let toc = outline(&body);
    let source = format!("{REPO}/blob/main/docs/{}.md", doc.slug);
    let mut contents = col().key("toc").role("navigation").label("Contents").w_full().gap(2.0).mt(20.0);
    if !toc.is_empty() {
        contents = contents.child(text("Contents").key("toc-h").font(BOLD).color(fg()).mb(4.0));
        for (i, (anchor, title)) in toc.iter().enumerate() {
            contents = contents.child(
                row()
                    .key(format!("toc-{anchor}"))
                    .gap(10.0)
                    .child(text(format!("{}", i + 1)).font(SMALL).color(fg2()).w(22.0))
                    .child(link_text(&format!("toc-{anchor}-a"), title, &format!("#{anchor}"), SMALL)),
            );
        }
    }
    col()
        .key("doc")
        .role("article")
        .w_full()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(text(doc.title().to_owned()).key("doc-h1").role("heading").level(1).font(bp.title()).color(fg()).wrap().selectable(true))
        .child(row().key("doc-meta").gap(18.0).mt(8.0).child(link_text("doc-all", "All docs", "/docs/", SMALL)).child(link_text(
            "doc-source",
            "Source",
            &source,
            SMALL,
        )))
        .child(contents)
        .child(col().key("doc-body").w_full().mt(24.0).child(markdown("doc-md", &body, mc)))
}
