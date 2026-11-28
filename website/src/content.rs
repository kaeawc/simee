//! What the site says, taken from the repository at build time: the README's sections for the
//! home page and `docs/*.md` for the docs. Nothing here carries a number of its own: every
//! measured claim the site shows is a line of the README or a doc, which quote
//! `docs/benchmarks.md` (AGENTS.md, "Benchmarks and public docs").

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

pub const REPO: &str = "https://github.com/kaeawc/simee";
pub const README: &str = include_str!("../../README.md");

/// One page of `docs/`, served at `/docs/{slug}/`.
pub struct Doc {
    pub slug: &'static str,
    pub group: &'static str,
    pub source: &'static str,
}

macro_rules! doc {
    ($group:literal, $slug:literal) => {
        Doc { slug: $slug, group: $group, source: include_str!(concat!("../../docs/", $slug, ".md")) }
    };
}

/// The docs the site publishes, in its sidebar's order. A doc not listed here (research notes,
/// design records) is linked on GitHub instead.
pub const DOCS: &[Doc] = &[
    doc!("Start", "comparison"),
    doc!("Start", "benchmarks"),
    doc!("Use", "cli"),
    doc!("Use", "api"),
    doc!("Use", "mcp"),
    doc!("Use", "desktop"),
    doc!("Use", "debugging"),
    doc!("Use", "streaming"),
    doc!("Use", "farm"),
    doc!("Use", "farm-android"),
    doc!("Platforms", "ios"),
    doc!("Platforms", "ios-versions"),
    doc!("Platforms", "android"),
    doc!("Platforms", "form-factors"),
    doc!("Platforms", "roadmap-android"),
    doc!("Build", "architecture"),
    doc!("Build", "development"),
    doc!("Build", "build"),
    doc!("Build", "automobile-tool-matrix"),
];

pub const GROUPS: [&str; 4] = ["Start", "Use", "Platforms", "Build"];

/// The images under `docs/` the site serves at `/img/{name}`.
pub const IMAGES: [&str; 2] = ["desktop.png", "desktop-android.png"];

impl Doc {
    pub fn find(slug: &str) -> Option<&'static Doc> {
        DOCS.iter().find(|d| d.slug == slug)
    }

    /// The doc's `# ` heading.
    pub fn title(&self) -> &'static str {
        self.source.lines().find_map(|l| l.strip_prefix("# ")).map_or(self.slug, str::trim)
    }

    /// The doc's body: its source without the title line, links resolved for the site.
    pub fn body(&self) -> String {
        let start = self.source.find('\n').filter(|_| self.source.starts_with("# ")).map_or(0, |i| i + 1);
        resolve_links(&self.source[start..], "docs")
    }

    /// The first paragraph under the title, as plain text: the page's description.
    pub fn summary(&self) -> String {
        first_paragraph(&self.body())
    }

    pub fn href(&self) -> String {
        format!("/docs/{}/", self.slug)
    }
}

/// The README's `## {heading}` section, its heading line included, links resolved for the site.
pub fn readme_section(heading: &str) -> String {
    resolve_links(&section(README, heading), "")
}

/// The README's first paragraph after its title: what simee is, in its own words.
pub fn readme_lede() -> String {
    let start = README.find('\n').map_or(0, |i| i + 1);
    first_paragraph(&README[start..])
}

/// [`readme_lede`] with its inline code still in backticks, for text that sets code in mono.
pub fn readme_lede_code() -> String {
    let start = README.find('\n').map_or(0, |i| i + 1);
    // Skip HTML blocks (the README's `<img>` mark) and images: the lede is the first prose paragraph.
    README[start..]
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty() && !p.starts_with('<') && !p.starts_with("!["))
        .unwrap_or("")
        .replace('\n', " ")
}

fn section(md: &str, heading: &str) -> String {
    let head = format!("## {heading}");
    let mut out = String::new();
    let mut inside = false;
    let mut fence = false;
    for line in md.lines() {
        if line.starts_with("```") {
            fence = !fence;
        }
        if !fence && line.starts_with("## ") {
            if inside {
                break;
            }
            inside = line.trim_end() == head;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn first_paragraph(md: &str) -> String {
    let mut text = String::new();
    let mut depth = 0;
    for event in Parser::new(md) {
        match event {
            Event::Start(Tag::Paragraph) => depth += 1,
            Event::End(TagEnd::Paragraph) if depth > 0 => break,
            Event::Text(t) | Event::Code(t) if depth > 0 => text.push_str(&t),
            Event::SoftBreak | Event::HardBreak if depth > 0 => text.push(' '),
            _ => {}
        }
    }
    text
}

/// `md` with every link and image destination outside code made to work on the site: a published
/// doc becomes its page, an image under `docs/` its `/img/` file, the README the home page, and
/// any other path in the repository its page on GitHub. `dir` is the directory `md` lives in,
/// relative to the repository root.
pub fn resolve_links(md: &str, dir: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fence = false;
    for line in md.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        if fence {
            out.push_str(line);
        } else {
            let line = if line.contains("](") { resolve_line(line, dir) } else { line.to_owned() };
            out.push_str(&link_code_paths(&line, dir));
        }
    }
    out
}

fn resolve_line(line: &str, dir: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut ticks = 0usize;
    while let Some(at) = rest.find("](") {
        let (before, after) = rest.split_at(at + 2);
        ticks += before.matches('`').count();
        out.push_str(before);
        let Some(close) = after.find(')') else {
            rest = after;
            break;
        };
        let target = &after[..close];
        // Inside inline code the text isn't a link.
        if ticks % 2 == 1 {
            out.push_str(target);
        } else {
            // A title (`[↗](docs/benchmarks.md#x "M3 Max, 2026-11-24")`) stays as it is.
            let target = target.trim_start();
            let (dest, title) = target.split_at(target.find(char::is_whitespace).unwrap_or(target.len()));
            out.push_str(&site_url(dest, dir));
            out.push_str(title);
        }
        rest = &after[close..];
    }
    out.push_str(rest);
    out
}

/// The top-level directories whose files a code span may name (`scripts/bench/android-boot.sh`).
/// `build.rs` keeps a copy.
const REPO_DIRS: [&str; 7] = ["scripts/", "crates/", "apps/", "desktop/", "website/", "tests/", "docs/"];

include!(concat!(env!("OUT_DIR"), "/missing_paths.rs"));

/// `line` with each code span that is a path in the repository (and nothing else) made a link
/// to it: on GitHub, or its page here for a published doc. A span already inside a link's text
/// stays as it is, and so does one naming a path that isn't there (`MISSING_PATHS`, from
/// `build.rs`): a removed script, or a path relative to a crate (`tests/publish.rs`).
fn link_code_paths(line: &str, dir: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let Some(len) = rest[open + 1..].find('`') else { break };
        let code = &rest[open + 1..open + 1 + len];
        let close = open + 1 + len + 1;
        out.push_str(&rest[..open]);
        let in_link = out.ends_with('[');
        let is_path = REPO_DIRS.iter().any(|d| code.starts_with(d))
            && code.len() > 4
            && code.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
            && !MISSING_PATHS.contains(&code);
        if is_path && !in_link {
            let rel = if dir.is_empty() { code.to_owned() } else { format!("{}/{code}", "../".repeat(dir.split('/').count())) };
            out.push_str(&format!("[`{code}`]({})", site_url(&rel, dir)));
        } else {
            out.push_str(&rest[open..close]);
        }
        rest = &rest[close..];
    }
    out.push_str(rest);
    out
}

/// The links in `md` that have a title, as (destination, title): the tooltip each one shows,
/// as GitHub shows it. Table 1's source icons name their host and date this way.
pub fn link_titles(md: &str) -> Vec<(String, String)> {
    Parser::new_ext(md, options())
        .filter_map(|event| match event {
            Event::Start(Tag::Link { dest_url, title, .. }) if !title.is_empty() => Some((dest_url.into_string(), title.into_string())),
            _ => None,
        })
        .collect()
}

/// `md` without its tables: a section quoted somewhere a table would repeat another page.
pub fn without_tables(md: &str) -> String {
    let mut out = String::new();
    let mut fence = false;
    for line in md.lines() {
        if line.starts_with("```") {
            fence = !fence;
        }
        if fence || !line.trim_start().starts_with('|') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Where a link to `target`, written in a file under `dir`, goes on the site.
pub fn site_url(target: &str, dir: &str) -> String {
    let target = target.trim();
    if let Some(anchor) = target.strip_prefix('#').filter(|a| !a.is_empty()) {
        return format!("#{}", site_anchor(anchor));
    }
    if target.is_empty() || target.contains("://") || target.starts_with("mailto:") || target.starts_with('/') {
        return target.to_owned();
    }
    let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
    // The site's pages key headings by `site_anchor`; GitHub keeps its own.
    let fragment = if anchor.is_empty() { String::new() } else { format!("#{}", site_anchor(anchor)) };
    let path = normalize(dir, path);
    if path == "README.md" {
        return format!("/{fragment}");
    }
    if let Some(name) = path.strip_prefix("docs/") {
        if let Some(slug) = name.strip_suffix(".md")
            && Doc::find(slug).is_some()
        {
            return format!("/docs/{slug}/{fragment}");
        }
        if IMAGES.contains(&name) {
            return format!("/img/{name}");
        }
    }
    let fragment = if anchor.is_empty() { String::new() } else { format!("#{anchor}") };
    format!("{REPO}/blob/main/{path}{fragment}")
}

/// A heading's anchor on the site: GitHub's, made an id the static page keeps. Rune's HTML export
/// drops an element id that doesn't start with an ASCII letter or has other than ASCII letters,
/// digits, `-` and `_` (`3d-device-view`, `1-boot-to-adb--done`), which left those headings with
/// no id and their contents entries with nowhere to go; such an anchor gets an `h-` prefix.
pub fn site_anchor(anchor: &str) -> String {
    let id: String = anchor.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if id.starts_with(|c: char| c.is_ascii_alphabetic()) { id } else { format!("h-{id}") }
}

/// `path` relative to `dir`, as a path from the repository root.
fn normalize(dir: &str, path: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for piece in path.split('/') {
        match piece {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// GitHub's anchor for a heading: lowercase, spaces to `-`, punctuation other than `-` and `_`
/// dropped.
pub fn slug(heading: &str) -> String {
    heading
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c.to_lowercase().next().unwrap_or(c)),
            _ => None,
        })
        .collect()
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH
}

/// One top-level heading of `md`: where its line starts, its level, its text and its anchor:
/// GitHub's (`-1`, `-2` on repeats, as GitHub does), as [`site_anchor`] keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct Heading {
    pub at: usize,
    pub level: HeadingLevel,
    pub text: String,
    pub anchor: String,
}

pub fn headings(md: &str) -> Vec<Heading> {
    let mut out: Vec<Heading> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<Heading> = None;
    for (event, range) in Parser::new_ext(md, options()).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0
                    && let Tag::Heading { level, .. } = tag
                {
                    let at = md[..range.start].rfind('\n').map_or(0, |i| i + 1);
                    current = Some(Heading { at, level, text: String::new(), anchor: String::new() });
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0
                    && let Some(mut h) = current.take()
                {
                    let base = slug(&h.text);
                    let n = seen.iter().filter(|s| **s == base).count();
                    seen.push(base.clone());
                    h.anchor = site_anchor(&if n == 0 { base } else { format!("{base}-{n}") });
                    out.push(h);
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(h) = current.as_mut() {
                    h.text.push_str(&t);
                }
            }
            _ => {}
        }
    }
    out
}

/// Keys for `Markdown::block_keys`: each heading keyed by its anchor, so `#performance-notes`
/// lands on it as it does on GitHub.
pub fn block_keys(md: &str) -> Vec<(usize, String)> {
    headings(md).into_iter().map(|h| (h.at, h.anchor)).collect()
}

/// The `##` headings of `md`, as (anchor, text): a doc page's outline.
pub fn outline(md: &str) -> Vec<(String, String)> {
    headings(md).into_iter().filter(|h| h.level == HeadingLevel::H2).map(|h| (h.anchor, h.text)).collect()
}

/// The `### ` subsections of `md`, as (heading, body without the heading line).
pub fn subsections(md: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut fence = false;
    for line in md.lines() {
        if line.starts_with("```") {
            fence = !fence;
        }
        if !fence && let Some(h) = line.strip_prefix("### ") {
            out.push((h.trim().to_owned(), String::new()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

/// The `## {heading}` section of the published doc `slug`, links resolved, without its heading.
pub fn doc_section(slug: &str, heading: &str) -> String {
    let Some(doc) = Doc::find(slug) else { return String::new() };
    let md = section(doc.source, heading);
    let body = md.split_once('\n').map_or("", |(_, rest)| rest);
    resolve_links(body, "docs")
}

/// The first Markdown table in `md`, keeping its header and only the rows whose first cell is in
/// `keep`.
pub fn table_rows(md: &str, keep: &[&str]) -> String {
    let rows: Vec<&str> = md.lines().skip_while(|l| !l.starts_with('|')).take_while(|l| l.starts_with('|')).collect();
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let first = row.trim_matches('|').split('|').next().unwrap_or("").trim();
        if i < 2 || keep.contains(&first) {
            out.push_str(row);
            out.push('\n');
        }
    }
    out
}

/// `md` up to the first line that starts with `marker`.
pub fn until(md: &str, marker: &str) -> String {
    md.lines().take_while(|l| !l.starts_with(marker)).map(|l| format!("{l}\n")).collect()
}
