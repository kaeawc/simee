use pulldown_cmark::{Event, Options, Parser, Tag};
use rune::{Env, Runtime};

use crate::content::{self, DOCS, Doc, block_keys, outline, resolve_links, site_url, slug};
use crate::tokens::Bp;
use crate::{Route, State, view};

fn page(route: Route, width: f32) -> Runtime<State> {
    let mut rt = Runtime::new(State::at(route), view, (width, 900.0), Env::new(true, 2.0));
    for _ in 0..3 {
        rt.frame(0.0);
    }
    rt
}

fn paths(rt: &Runtime<State>) -> Vec<String> {
    rt.elements().into_iter().filter(|e| !e.exiting).map(|e| e.path).collect()
}

/// The static page keeps the narrowest width's tree and switches styles with `@media`, so every
/// width a page is exported at must build the same elements.
#[test]
fn every_page_builds_the_same_tree_at_every_breakpoint() {
    let widths: Vec<f32> = Bp::EXPORT.split(',').map(|w| w.parse().unwrap()).chain([1400.0]).collect();
    for (path, route) in Route::all().into_iter().chain([("/404".to_owned(), Route::NotFound)]) {
        let narrow = paths(&page(route, widths[0]));
        assert!(narrow.iter().any(|p| p.ends_with("main")), "{path} has no main: {narrow:?}");
        for &w in &widths[1..] {
            let wide = paths(&page(route, w));
            let only: Vec<_> = wide.iter().filter(|p| !narrow.contains(p)).chain(narrow.iter().filter(|p| !wide.contains(p))).collect();
            assert!(only.is_empty(), "{path} at {w}: {only:?}");
        }
    }
}

#[test]
fn routes_parse_with_and_without_slashes() {
    assert_eq!(Route::parse("/"), Route::Home);
    assert_eq!(Route::parse("/index.html"), Route::Home);
    assert_eq!(Route::parse("/docs"), Route::Docs);
    assert_eq!(Route::parse("/docs/android/"), Route::Doc(Doc::find("android").unwrap()));
    assert_eq!(Route::parse("/docs/android/index.html"), Route::Doc(Doc::find("android").unwrap()));
    assert_eq!(Route::parse("/docs/research/"), Route::NotFound);
    assert_eq!(Route::parse("/devices/"), Route::Devices);
    assert_eq!(Route::parse("/nope"), Route::NotFound);
}

#[test]
fn every_published_route_parses_back_to_itself() {
    for (path, route) in Route::all() {
        assert_eq!(Route::parse(&path), route, "{path}");
    }
}

#[test]
fn every_doc_has_a_title_and_a_summary() {
    for doc in DOCS {
        assert_ne!(doc.title(), doc.slug, "{} has no # title", doc.slug);
        assert!(!doc.summary().is_empty(), "{} has no first paragraph", doc.slug);
    }
}

#[test]
fn links_resolve_to_site_pages_images_and_github() {
    assert_eq!(site_url("android.md#performance-notes", "docs"), "/docs/android/#performance-notes");
    assert_eq!(site_url("docs/benchmarks.md#frame-pacing", ""), "/docs/benchmarks/#frame-pacing");
    assert_eq!(site_url("desktop.png", "docs"), "/img/desktop.png");
    assert_eq!(site_url("docs/desktop.png", ""), "/img/desktop.png");
    assert_eq!(site_url("../README.md", "docs"), "/");
    assert_eq!(site_url("research/bluetooth.md", "docs"), "https://github.com/kaeawc/simee/blob/main/docs/research/bluetooth.md");
    assert_eq!(
        site_url("../crates/sim-core/src/form_factor.rs", "docs"),
        "https://github.com/kaeawc/simee/blob/main/crates/sim-core/src/form_factor.rs"
    );
    assert_eq!(site_url("https://example.com/a.md", "docs"), "https://example.com/a.md");
    assert_eq!(site_url("#toolsets", "docs"), "#toolsets");
}

#[test]
fn link_titles_survive_resolving() {
    // Table 1's source icons carry their host and date as a link title; the site resolved the
    // destination with the title still in it, which made the anchor junk.
    let md = "[\u{2197}](docs/benchmarks.md#frame-pacing \"Android: M3 Max, 2026-11-25\")";
    let out = resolve_links(md, "");
    assert_eq!(out, "[\u{2197}](/docs/benchmarks/#frame-pacing \"Android: M3 Max, 2026-11-25\")");
    assert_eq!(content::link_titles(&out), [("/docs/benchmarks/#frame-pacing".to_owned(), "Android: M3 Max, 2026-11-25".to_owned())]);
}

#[test]
fn table_1_has_measures_as_rows_and_a_source_icon_per_platform() {
    let perf = content::readme_section("Performance");
    let header = perf.lines().find(|l| l.starts_with('|')).expect("README ## Performance has a table");
    let cells: Vec<&str> = header.trim_matches('|').split('|').map(str::trim).collect();
    assert_eq!(cells, ["Measure", "simee, Android / iOS", "Android emulator (QEMU)", "Simulator.app", "Source"]);
    // Each source is an icon whose tooltip names the platform, host and date, not file names in
    // the cell.
    let rt = page(Route::Home, 1400.0);
    let in_table = |e: &rune::ElementInfo| !e.exiting && e.path.contains("t1-md");
    let sources: Vec<_> = rt.elements().into_iter().filter(|e| in_table(e) && e.role == "link").collect();
    assert!(sources.len() >= 2, "Table 1 has no source links");
    for e in &sources {
        let help = e.help.as_deref().unwrap_or_else(|| panic!("{:?} has no tooltip", e.key));
        assert!(help.contains("2026-") && (help.starts_with("Android") || help.starts_with("iOS")), "{help}");
    }
    assert!(sources.iter().any(|e| e.help.as_deref().is_some_and(|h| h.starts_with("iOS"))), "no iOS source");
    let table: String = perf.lines().filter(|l| l.starts_with('|')).map(|l| format!("{l}\n")).collect();
    for event in Parser::new_ext(&table, Options::ENABLE_TABLES) {
        if let Event::Text(t) | Event::Code(t) = event {
            assert!(!t.contains("scripts/") && !t.contains(".md"), "a file name shows in Table 1: {t}");
        }
    }
}

#[test]
fn links_in_code_are_left_alone() {
    let md = "See [cli](cli.md) and `[x](cli.md)`.\n```\n[y](cli.md)\n```\n";
    assert_eq!(resolve_links(md, "docs"), "See [cli](/docs/cli/) and `[x](cli.md)`.\n```\n[y](cli.md)\n```\n");
}

#[test]
fn no_published_doc_links_to_a_relative_path() {
    // Every relative link in a published doc becomes a site page, a site image or GitHub.
    for doc in DOCS {
        let body = doc.body();
        for (i, _) in body.match_indices("](") {
            let target = &body[i + 2..];
            let target = &target[..target.find(')').unwrap_or(target.len())];
            let ok = target.starts_with('/')
                || target.starts_with('#')
                || target.contains("://")
                || target.starts_with("mailto:")
                || target.contains('`')
                || target.contains(' ');
            assert!(ok || body[..i].matches("```").count() % 2 == 1, "{}: {target}", doc.slug);
        }
    }
}

#[test]
fn anchors_match_github() {
    assert_eq!(slug("Performance notes"), "performance-notes");
    assert_eq!(slug("Boot to `sys.boot_completed` with emu's devices"), "boot-to-sysboot_completed-with-emus-devices");
    assert_eq!(slug("API 29 and 30: first boots and idle host CPU"), "api-29-and-30-first-boots-and-idle-host-cpu");
    let keys = block_keys("# A\n\npara\n\n## B\n\n- x\n\n## B\n\n```\n## not\n```\n");
    assert_eq!(keys, [(0, "a".to_owned()), (11, "b".to_owned()), (22, "b-1".to_owned())]);
    assert_eq!(outline("## One\n\ntext\n\n## Two `x`\n"), [("one".to_owned(), "One".to_owned()), ("two-x".to_owned(), "Two x".to_owned())]);
}

#[test]
fn the_readme_sections_the_home_page_shows_exist() {
    for heading in crate::home::README_SECTIONS.into_iter().chain(["Compared with the stock tools", "Not yet"]) {
        let md = content::readme_section(heading);
        assert!(md.lines().count() > 2, "README has no ## {heading}");
    }
    assert!(content::readme_lede().starts_with("iOS simulators and Android emulators"));
    // The abstract is prose, never the README's HTML (its `<img>` mark showed as text).
    assert!(content::readme_lede_code().starts_with("iOS simulators and Android emulators"), "{}", content::readme_lede_code());
}

#[test]
fn themes_cycle_and_round_trip() {
    use crate::theme::Theme;
    let mut t = Theme::System;
    for want in [Theme::Light, Theme::Dark, Theme::System] {
        t = t.next();
        assert_eq!(t, want);
        assert_eq!(Theme::parse(t.as_str()), t);
    }
    assert!(Theme::System.dark(true) && !Theme::System.dark(false));
    assert!(Theme::Dark.dark(false) && !Theme::Light.dark(true));
}

#[test]
fn rune_starts_from_the_theme_the_page_already_shows() {
    use crate::theme::Theme;
    // The head script's data-theme wins; localStorage covers a page where it didn't run.
    assert_eq!(Theme::chosen(Some("dark"), Some("light")), Theme::Dark);
    assert_eq!(Theme::chosen(None, Some("light")), Theme::Light);
    assert_eq!(Theme::chosen(Some("bogus"), Some("dark")), Theme::Dark);
    assert_eq!(Theme::chosen(None, Some("system")), Theme::System);
    assert_eq!(Theme::chosen(None, None), Theme::System);
}

/// Every page is `index.html` filled in (scripts/build.sh), so its head script is every page's.
#[test]
fn the_saved_theme_applies_before_anything_paints() {
    let template = include_str!("../index.html");
    let head = &template[..template.find("</head>").unwrap()];
    let script = head.find("localStorage.getItem(\"simee-theme\")").expect("the head reads the saved theme");
    // Before any stylesheet, font or the export's CSS, so the first paint is already themed.
    for later in ["<style", "<link", "<!-- rune:css -->"] {
        assert!(head.find(later).is_some_and(|at| at > script), "{later} comes before the theme script");
    }
    assert!(head[script..].contains("document.documentElement.dataset.theme = t"));
    // The static toggle is found by the label Rune exports for it (ui::theme_toggle), and the
    // build checks every page it writes still has the script.
    assert!(head.contains("[aria-label^=\"Theme: \"]"));
    assert!(include_str!("ui.rs").contains(".label(format!(\"Theme: {}\", theme.label()))"));
    assert!(include_str!("../scripts/build.sh").contains("simee-theme"));
}

#[test]
fn code_spans_naming_repo_paths_link_to_them() {
    let md = "rerun with `scripts/bench/android-boot.sh`, see `docs/cli.md`, not `simee list` or [`scripts/x.sh`](https://example.com/y)";
    let out = resolve_links(md, "");
    assert!(
        out.contains("[`scripts/bench/android-boot.sh`](https://github.com/kaeawc/simee/blob/main/scripts/bench/android-boot.sh)"),
        "{out}"
    );
    assert!(out.contains("[`docs/cli.md`](/docs/cli/)"), "{out}");
    assert!(out.contains("`simee list`") && out.contains("[`scripts/x.sh`](https://example.com/y)"), "{out}");
    // From a doc, the path is still the repository's.
    assert!(resolve_links("`scripts/lint.sh`", "docs").contains("(https://github.com/kaeawc/simee/blob/main/scripts/lint.sh)"));
}

#[test]
fn quick_start_drops_its_table() {
    let quick = content::without_tables(&content::readme_section("Quick start"));
    assert!(!quick.lines().any(|l| l.starts_with('|')), "{quick}");
    assert!(quick.contains("simee doctor"), "{quick}");
}

/// Every page's Markdown, as (where, Markdown): the published docs, the README sections the
/// pages quote and the debugging page's section of its doc.
fn page_markdown() -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = DOCS.iter().map(|d| (d.href(), d.body())).collect();
    for heading in crate::home::README_SECTIONS.into_iter().chain(["Compared with the stock tools", "Not yet"]) {
        all.push((format!("README ## {heading}"), content::readme_section(heading)));
    }
    all.push(("/debugging/".to_owned(), content::doc_section("debugging", "iOS debug surface")));
    all
}

#[test]
fn every_image_a_page_shows_is_served() {
    // The desktop doc's `desktop.png` showed its alt text: the site resolved it but the Markdown
    // had no picture for it. Every image is one the build copies to /img/ and the app draws.
    let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs");
    for name in content::IMAGES {
        assert!(std::path::Path::new(&format!("{docs}/{name}")).is_file(), "docs/{name} is missing");
    }
    let mut seen = 0;
    for (page, md) in page_markdown() {
        for event in Parser::new_ext(&md, Options::ENABLE_TABLES) {
            match event {
                Event::Start(Tag::Image { dest_url, .. }) => {
                    let name = dest_url.strip_prefix("/img/").unwrap_or_else(|| panic!("{page}: image {dest_url} isn't served from /img/"));
                    assert!(content::IMAGES.contains(&name), "{page}: {name} isn't in content::IMAGES, so the build doesn't copy it");
                    seen += 1;
                }
                Event::Html(h) | Event::InlineHtml(h) if h.contains("<img") => panic!("{page}: an HTML <img> shows as text: {h}"),
                _ => {}
            }
        }
    }
    assert!(seen > 0, "no page shows an image: the check found nothing to check");
    // The home page memoizes its quoted sections without the pictures' state: they have none.
    for heading in crate::home::README_SECTIONS {
        assert!(!content::readme_section(heading).contains("!["), "README ## {heading} has an image");
    }
}

#[test]
fn a_docs_image_draws_its_picture() {
    let doc = Doc::find("desktop").unwrap();
    let alt = "The desktop window with iOS and Android devices";
    assert!(doc.source.contains(&format!("![{alt}](desktop.png)")), "docs/desktop.md's image changed: update this test");
    let mut rt = Runtime::new(State::with_images(Route::Doc(doc)), view, (760.0, 900.0), Env::new(true, 2.0));
    let shows_alt_text = |rt: &Runtime<State>| rt.elements().iter().any(|e| !e.exiting && e.text.as_deref() == Some(alt));
    let mut drawn = false;
    for i in 0..200 {
        // The cache's waker does nothing here: build the view again each frame, as a wake would.
        rt.invalidate();
        rt.frame(f64::from(i) / 60.0);
        drawn = !shows_alt_text(&rt) && rt.elements().iter().any(|e| e.role == "image" && e.label.as_deref() == Some(alt));
        if drawn {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(drawn, "the image still shows its alt text in place of the picture");
}

#[test]
fn no_page_shows_markdown_or_html_as_text() {
    for (path, route) in Route::all().into_iter().chain([("/404".to_owned(), Route::NotFound)]) {
        for e in page(route, 760.0).elements() {
            let Some(text) = e.text.as_deref() else { continue };
            for raw in ["<img", "](", "![", "<br", "<details", "<a href"] {
                assert!(!text.contains(raw), "{path}: {raw} shows as text in {}: {text:.120}", e.path);
            }
        }
    }
}

#[test]
fn every_in_site_anchor_lands_on_a_heading() {
    // `#3d-device-view` and the roadmap's `#1-boot-to-adb--done` went nowhere: Rune's export
    // drops an id that starts with a digit, so the heading had none.
    let anchors = |md: &str| content::headings(md).into_iter().map(|h| h.anchor).collect::<Vec<_>>();
    for doc in DOCS {
        let body = doc.body();
        for a in anchors(&body) {
            let id =
                a.starts_with(|c: char| c.is_ascii_alphabetic()) && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            assert!(id, "{}: heading anchor {a} isn't an id the static page keeps", doc.slug);
        }
        for event in Parser::new_ext(&body, Options::ENABLE_TABLES) {
            let Event::Start(Tag::Link { dest_url, .. }) = event else { continue };
            let (target, anchor) = match dest_url.split_once('#') {
                Some(("", a)) => (doc.slug, a),
                Some((p, a)) if p.starts_with("/docs/") => (p.trim_start_matches("/docs/").trim_end_matches('/'), a),
                _ => continue,
            };
            let Some(to) = Doc::find(target) else { continue };
            assert!(anchors(&to.body()).iter().any(|x| x == anchor), "{}: {dest_url} lands on no heading of {target}", doc.slug);
        }
    }
    assert_eq!(content::site_anchor("3d-device-view"), "h-3d-device-view");
    assert_eq!(content::site_anchor("performance-notes"), "performance-notes");
    assert_eq!(site_url("#3d-device-view", "docs"), "#h-3d-device-view");
    assert_eq!(site_url("roadmap-android.md#1-boot-to-adb--done", "docs"), "/docs/roadmap-android/#h-1-boot-to-adb--done");
    // GitHub keeps its own anchors.
    assert_eq!(site_url("research/x.md#1-a", "docs"), "https://github.com/kaeawc/simee/blob/main/docs/research/x.md#1-a");
}

#[test]
fn every_github_link_names_a_path_in_the_repository() {
    // Code spans such as the streaming doc's `tests/publish.rs` (its crate's) and a removed
    // `scripts/ios/xcode-build.sh` became links to 404s.
    let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
    let mut checked = 0;
    for (page, md) in page_markdown() {
        for event in Parser::new_ext(&md, Options::ENABLE_TABLES) {
            let Event::Start(Tag::Link { dest_url, .. }) = event else { continue };
            let Some(path) = dest_url.strip_prefix(&format!("{}/blob/main/", content::REPO)) else { continue };
            let path = path.split('#').next().unwrap_or(path).trim_end_matches('/');
            assert!(std::path::Path::new(&format!("{repo}/{path}")).exists(), "{page}: {dest_url} names no file in the repository");
            checked += 1;
        }
    }
    assert!(checked > 0);
    assert!(!resolve_links("`tests/publish.rs`", "docs").contains("]("), "a crate-relative path linked");
}

#[test]
fn the_static_page_keeps_links_and_long_code_in_the_line() {
    // Rune's export makes every element flex and only a text's own children inline: a link in a
    // paragraph stood on a line of its own, and a long code span widened a phone's page.
    let template = include_str!("../index.html");
    assert!(template.contains("[data-rune-static] .t a span { display: inline; position: static; }"));
    assert!(template.contains("[data-rune-static] .t { overflow-wrap: break-word; }"));
}

#[test]
fn every_page_fits_a_phone() {
    // At phone width the running head's links didn't wrap, long link labels and tables ran past
    // the column and Figure 1's last label past the page: every page scrolled sideways. Only
    // what is inside a sideways-scrolling viewport (a wide table's rows) may be wider.
    let width = 360.0;
    for (path, route) in Route::all().into_iter().chain([("/404".to_owned(), Route::NotFound)]) {
        let wide: Vec<String> = page(route, width)
            .elements()
            .into_iter()
            .filter(|e| !e.exiting && e.rect[2] > 0.0 && e.rect[0] + e.rect[2] > width + 0.5 && !e.path.contains("/scroll/"))
            .map(|e| format!("{} {:?}", e.path, e.rect))
            .collect();
        assert!(wide.is_empty(), "{path} is wider than {width}: {:#?}", &wide[..wide.len().min(8)]);
    }
}

#[test]
fn further_reading_citations_use_the_full_width() {
    // The list and its rows shrank to their content, so each link (basis 0, grow 1) laid out at
    // its min-content width: one word per line. At desktop width each citation is one line; on a
    // phone it may wrap, but beside its `[n]` marker and within the column.
    let rect = |rt: &Runtime<State>, suffix: &str| {
        rt.elements().into_iter().find(|e| !e.exiting && e.path.ends_with(suffix)).unwrap_or_else(|| panic!("no {suffix}")).rect
    };
    for (width, one_line) in [(1084.0, true), (1400.0, true), (360.0, false)] {
        let rt = page(Route::Home, width);
        let list = rect(&rt, "/refs-list");
        let single = rect(&rt, "/ref-5-a")[3];
        for i in 0..6 {
            let row = rect(&rt, &format!("/ref-{i}-row"));
            let link = rect(&rt, &format!("/ref-{i}-a"));
            assert!((row[2] - list[2]).abs() < 0.5, "ref {i} row at {width}: {row:?} in {list:?}");
            assert!(link[0] + link[2] <= list[0] + list[2] + 0.5, "ref {i} at {width} overflows: {link:?}");
            if one_line {
                assert!(link[3] <= single + 0.5, "ref {i} wraps at {width}: {link:?}, one line is {single}");
            } else {
                assert!(link[3] <= single * 3.0 + 0.5, "ref {i} wraps word by word at {width}: {link:?}");
            }
        }
    }
}
