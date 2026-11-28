//! The frame every page shares (a running head and a footer rule), numbered sections, captions
//! and Markdown in the paper's style.

use rune::{Align, Color, El, LineCap, Shape, Span, TextStyle, Tx, canvas, col, el, oklch, rich, row, text};
use rune_kit::{Markdown, MarkdownStyle};

use crate::content::{self, REPO};
use crate::gem;
use crate::theme::Theme;
use crate::tokens::*;
use crate::{Pictures, Route, State};

/// `s` with each `` `code` `` in mono.
pub fn prose(key: &str, s: &str, style: TextStyle, color: rune::Color) -> El {
    let mut content = String::with_capacity(s.len());
    let mut spans = Vec::new();
    for (i, piece) in s.split('`').enumerate() {
        let at = content.len();
        content.push_str(piece);
        if i % 2 == 1 {
            spans.push(Span::new(at, content.len()).mono().size(style.size - 2.0));
        }
    }
    rich(content, spans).key(key).font(style).color(color).wrap().selectable(true)
}

/// A link in running text or a list: the gem's green, underlined under the pointer. A label
/// longer than its line wraps (a contents entry on a phone).
pub fn link_text(key: &str, label: &str, href: &str, style: TextStyle) -> El {
    row()
        .key(key)
        .href(href)
        .hover_group()
        .shrink()
        .child(text(label.to_owned()).font(style).color(link()).group_color(fg()).wrap().shrink())
}

/// The running head: the mark and name, then the pages and the theme toggle, over a rule.
pub fn topbar(route: &Route, theme: Theme, angle: f32) -> El {
    let here = |r: fn(&Route) -> bool| r(route);
    let item = |key: &str, label: &str, href: &str, current: bool| {
        row()
            .key(key.to_owned())
            .href(href)
            .hover_group()
            .py(4.0)
            .child(text(label.to_owned()).font(SMALL).color(if current { fg() } else { fg2() }).group_color(fg()))
    };
    let pages = row()
        .key("nav")
        .role("navigation")
        .label("simee")
        .gap(16.0)
        .flex_wrap()
        .center_y()
        // Narrower than its items (a phone), it wraps them instead of widening the page.
        .shrink()
        .child(item("nav-benchmarks", "Benchmarks", "/benchmarks/", here(|r| matches!(r, Route::Benchmarks))))
        .child(item("nav-debugging", "Debugging", "/debugging/", here(|r| matches!(r, Route::Debugging))))
        .child(item("nav-devices", "Devices", "/devices/", here(|r| matches!(r, Route::Devices))))
        .child(item("nav-demos", "Demos", "/demos/", here(|r| matches!(r, Route::Demos))))
        .child(item("nav-limitations", "Limitations", "/limitations/", here(|r| matches!(r, Route::Limitations))))
        .child(item("nav-docs", "Docs", "/docs/", here(|r| matches!(r, Route::Docs | Route::Doc(_)))))
        .child(item("nav-source", "Source", REPO, false))
        .child(theme_toggle(theme));
    let brand = row()
        .key("brand")
        .href("/")
        .label("simee home")
        .gap(8.0)
        .center_y()
        .child(gem::mark(18.0, angle))
        .child(text("simee").font(BOLD).color(fg()));
    col()
        .key("top")
        .role("banner")
        .w_full()
        .pt(16.0)
        .child(row().key("top-row").w_full().justify_between().center_y().gap(16.0).flex_wrap().pb(10.0).child(brand).child(pages))
        .child(el().key("top-rule").w_full().h(1.0).fill(rule()).a11y_hidden())
}

/// The theme toggle: a half-lit disc and what the page follows (System, Light, Dark); a press
/// moves to the next. Without Rune, index.html's script does the same to the static page.
fn theme_toggle(theme: Theme) -> El {
    let disc = Shape::new()
        .view_box(12.0, 12.0)
        .stroke_svg("M6 0.75 A5.25 5.25 0 1 1 6 11.25 A5.25 5.25 0 1 1 6 0.75 Z", 1.0, LineCap::Round, fg2())
        .fill_svg("M6 0.75 A5.25 5.25 0 0 1 6 11.25 Z", fg2());
    row()
        .key("theme")
        .role("button")
        .label(format!("Theme: {}", theme.label()))
        .focusable(true)
        .hover_group()
        .gap(6.0)
        .center_y()
        .py(4.0)
        .on_press(|s: &mut State| s.cycle_theme())
        .child(canvas(disc).key("theme-disc").size(12.0, 12.0).a11y_hidden())
        .child(text(theme.label()).key("theme-label").font(SMALL).color(fg2()).group_color(fg()))
}

pub fn footer() -> El {
    col()
        .key("footer")
        .role("contentinfo")
        .w_full()
        .pt(56.0)
        .pb(40.0)
        .gap(10.0)
        .child(el().key("footer-rule").w(120.0).h(1.0).fill(rule()).a11y_hidden())
        .child(
            row()
                .key("footer-row")
                .gap(18.0)
                .flex_wrap()
                .child(text("MIT licence. Typeset by Rune.").key("footer-note").font(SMALL).color(fg2()))
                .child(link_text("footer-issues", "Issues", &format!("{REPO}/issues"), SMALL))
                .child(link_text("footer-changelog", "Changelog", &format!("{REPO}/blob/main/CHANGELOG.md"), SMALL)),
        )
}

/// A numbered section: "1 Performance", its anchor at `id`.
pub fn section(id: &str, number: &str, title: &str, bp: Bp) -> El {
    let label = if number.is_empty() { title.to_owned() } else { format!("{number}\u{2003}{title}") };
    col()
        .key(id)
        .role("region")
        .label(title)
        .w_full()
        .pt(if bp.phone() { 32.0 } else { 44.0 })
        .child(text(label).key(format!("{id}-h")).role("heading").level(2).font(bp.h2()).color(fg()).wrap().selectable(true).mb(10.0))
}

/// A table or figure's caption, set as papers set them: "Table 1." in bold, then the text.
pub fn caption(key: &str, label: &str, s: &str) -> El {
    let content = format!("{label}. {s}");
    let spans = vec![Span::new(0, label.len() + 1).weight(700.0)];
    rich(content, spans).key(key).font(SMALL).color(fg()).wrap().selectable(true).mt(18.0).mb(6.0)
}

/// Centered text, for the title block.
pub fn centered(key: &str, s: &str, style: TextStyle, color: rune::Color) -> El {
    text(s.to_owned()).key(key).font(style).color(color).wrap().selectable(true).align(Align::Center).w_full()
}

/// The Markdown style every page uses: Rune's neutral one for the theme, in CMU Serif with the
/// paper's colours, paragraphs at the text column's width and tables across the page.
pub fn markdown_style() -> MarkdownStyle {
    MarkdownStyle::neutral(dark())
        .prose_width(MEASURE)
        .text(fg())
        .muted(fg2())
        .link(link())
        .link_hover(fg())
        .body_font(BODY)
        .heading_font(1, TextStyle::new(24.0, 700.0).lh(32.0))
        .heading_font(2, TextStyle::new(21.0, 700.0).lh(28.0))
        .heading_font(3, TextStyle::new(18.0, 700.0).lh(26.0))
        .heading_font(4, BOLD)
        .code_block(code_bg(), rule_faint())
        .code_block_font(MONO_13)
        .inline_code_bg(code_bg())
        .rule(rule_faint())
        .strong_weight(700.0)
        // Tables as papers set them: no box or fill, a rule under the header, faint rules between
        // rows, figures in tabular widths.
        .table_outline(None)
        .table_header_fill(Color::TRANSPARENT)
        .table_header_rule(rule())
        .table_header_weight(700.0)
        .table_rules(if dark() { oklch(0.26, 0.0, 0.0) } else { oklch(0.9, 0.0, 0.0) })
        .table_tabular_figures(true)
        .table_flush_left(true)
        .table_full_width(true)
}

/// What the site's Markdown needs from the view ([`crate::md_cx`]).
#[derive(Clone)]
pub struct MdCx {
    /// The images it can draw.
    pub pictures: Pictures,
    /// The width of the column it lays out in.
    pub column: f32,
}

/// `md` as the site draws Markdown: headings keyed by their anchors, images drawn from the
/// site's `/img/`, tables sized in the column as a browser sizes them and scrolling sideways
/// where even their narrowest is wider (a phone), code wrapping, and links that open in this
/// tab when they are the site's own and in a new one otherwise, with their title as a tooltip.
pub fn markdown(key: &str, md: &str, mc: &MdCx) -> El {
    let pictures = mc.pictures.clone();
    let titles = content::link_titles(md);
    Markdown::new(md)
        .block_keys(content::block_keys(md))
        .images(move |src| pictures.src(src))
        .scroll_wide(true)
        .wrap_code(true)
        .on_link_tx(|_: &mut State, url: &str, tx: &mut Tx| follow(url, tx))
        .link_help(move |url| titles.iter().find(|(dest, _)| dest == url).map(|(_, title)| title.clone()))
        .render(&markdown_style().text_width(mc.column))
        .key(key.to_owned())
        .w_full()
}

/// Opens `url`: the site's own pages in this tab, other sites in a new one.
pub fn follow(url: &str, tx: &mut Tx) {
    if url.starts_with('/') || url.starts_with('#') {
        tx.open_url_same_tab(url).ok();
    } else {
        tx.open_url(url).ok();
    }
}
