//! simee's public website, drawn by Rune. Each page is this app in web document mode: the
//! browser scrolls a semantic mirror of it (real headings, paragraphs and links), and the canvas
//! draws the band in view. `--html` exports the same tree as each page's static first frame
//! (`scripts/build.sh`), which is what crawlers, readers without WebGPU and readers without
//! JavaScript get.
//!
//! The route comes from the page's path in the browser and from `SIMEE_WEB_ROUTE` when exporting
//! natively. Links between pages are ordinary links: each page is its own HTML file.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use rune::{AppSpec, Cx, El, Image, ImageCache, SurfaceFit, Tx, col, el};

pub mod content;
mod demos;
mod devices;
mod docs;
mod gem;
mod home;
mod pages;
mod theme;
mod tokens;
mod ui;

use content::Doc;
use theme::Theme;
use tokens::*;

/// The widths each page's static HTML is laid out at (`--html-widths`).
pub const EXPORT_WIDTHS: &str = tokens::Bp::EXPORT;

pub const TITLE: &str = "simee · Rust iOS sim and Android emu - Fast & Feature Rich";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Home,
    Benchmarks,
    Debugging,
    Limitations,
    Docs,
    Doc(&'static Doc),
    Devices,
    Demos,
    NotFound,
}

impl PartialEq for Doc {
    fn eq(&self, other: &Self) -> bool {
        self.slug == other.slug
    }
}
impl Eq for Doc {}
impl std::fmt::Debug for Doc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug)
    }
}

impl Route {
    /// The page at `path` (`/`, `/docs/`, `/docs/android/`, `/devices/`, `/demos/`), with or
    /// without its trailing slash or `index.html`.
    pub fn parse(path: &str) -> Route {
        let path = path.trim_end_matches("index.html");
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        match parts.as_slice() {
            [] => Route::Home,
            ["benchmarks"] => Route::Benchmarks,
            ["debugging"] => Route::Debugging,
            ["limitations"] => Route::Limitations,
            ["docs"] => Route::Docs,
            ["docs", slug] => Doc::find(slug).map_or(Route::NotFound, Route::Doc),
            ["devices"] => Route::Devices,
            ["demos"] => Route::Demos,
            _ => Route::NotFound,
        }
    }

    /// Every page the site publishes, as (path, route): what `scripts/build.sh` exports.
    pub fn all() -> Vec<(String, Route)> {
        let mut all = vec![
            ("/".to_owned(), Route::Home),
            ("/benchmarks/".to_owned(), Route::Benchmarks),
            ("/debugging/".to_owned(), Route::Debugging),
            ("/limitations/".to_owned(), Route::Limitations),
            ("/docs/".to_owned(), Route::Docs),
        ];
        all.extend(content::DOCS.iter().map(|d| (d.href(), Route::Doc(d))));
        all.push(("/devices/".to_owned(), Route::Devices));
        all.push(("/demos/".to_owned(), Route::Demos));
        all
    }

    /// The page's `<title>`.
    pub fn title(&self) -> String {
        match self {
            Route::Home => TITLE.to_owned(),
            Route::Benchmarks => "Benchmarks · simee".to_owned(),
            Route::Debugging => "Debugging · simee".to_owned(),
            Route::Limitations => "Limitations · simee".to_owned(),
            Route::Docs => "Docs · simee".to_owned(),
            Route::Doc(d) => format!("{} · simee docs", d.title()),
            Route::Devices => "Device concepts · simee".to_owned(),
            Route::Demos => "Demos · simee".to_owned(),
            Route::NotFound => "Not found · simee".to_owned(),
        }
    }

    /// The page's meta description.
    pub fn description(&self) -> String {
        match self {
            Route::Home | Route::NotFound => content::readme_lede(),
            Route::Benchmarks => "simee against the Android SDK's emulator, Simulator.app and simctl: boot times, memory, frame pacing, idle cost and command latency, with how each was measured.".to_owned(),
            Route::Debugging => "What simee exposes on a simulated iOS device that the stock tools don't: processes, sockets, containers, preferences, logs, rendering overlays and device state.".to_owned(),
            Route::Limitations => "What simee doesn't do yet.".to_owned(),
            Route::Docs => "simee's documentation: the comparison with the stock tools, benchmarks, the CLI, API and MCP server, both platforms' backends and the architecture.".to_owned(),
            Route::Doc(d) => d.summary(),
            Route::Devices => "Every form factor simee supports on iOS and Android, with each kind's reference profile and inputs.".to_owned(),
            Route::Demos => "The simee desktop window with iOS and Android devices, and how to run it.".to_owned(),
        }
    }
}

/// The route this process draws: the browser's path, or `SIMEE_WEB_ROUTE` when exporting.
fn current_route() -> Route {
    #[cfg(target_arch = "wasm32")]
    let path = web_sys::window().and_then(|w| w.location().pathname().ok()).unwrap_or_default();
    #[cfg(not(target_arch = "wasm32"))]
    let path = rune::env::var("SIMEE_WEB_ROUTE").unwrap_or_else(|| "/".to_owned());
    Route::parse(&path)
}

pub struct State {
    route: Route,
    /// The images, once Rune hands the app its waker.
    gallery: Option<Rc<Gallery>>,
    /// The frame clock, in seconds: what turns the gem.
    t: f64,
    /// Light, dark or the system's.
    theme: Theme,
}

impl State {
    fn new() -> State {
        State { route: current_route(), gallery: None, t: 0.0, theme: theme::load() }
    }

    #[cfg(test)]
    fn at(route: Route) -> State {
        State { route, gallery: None, t: 0.0, theme: Theme::System }
    }

    /// [`State::at`] with images read from the repository's `docs/`, as a page draws its
    /// pictures once they load.
    #[cfg(test)]
    fn with_images(route: Route) -> State {
        let cache = ImageCache::new(std::sync::Arc::new(|| {}));
        State { gallery: Some(Rc::new(Gallery::new(cache, concat!(env!("CARGO_MANIFEST_DIR"), "/../docs")))), ..State::at(route) }
    }

    /// The pictures this page can draw, at the window's `scale`.
    fn pictures(&self, scale: f32) -> Pictures {
        Pictures { gallery: self.gallery.clone(), scale }
    }

    /// The toggle in the running head: the next theme, kept for the next page.
    fn cycle_theme(&mut self) {
        self.theme = self.theme.next();
        theme::store(self.theme);
    }
}

/// The images under `docs/` the site serves at `/img/{name}` (`content::IMAGES`).
struct Gallery {
    cache: ImageCache,
    /// Where the native build reads them: `img` beside the export (the build runs in the output
    /// folder), the repository's `docs` in tests.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    dir: &'static str,
    /// The handles Markdown images draw from. A handle no element holds lets its image go, and
    /// an image that hasn't loaded yet shows its alt text, which holds nothing: without these,
    /// each frame asked for it again and it never loaded.
    held: RefCell<Vec<(&'static str, Image)>>,
}

impl Gallery {
    fn new(cache: ImageCache, dir: &'static str) -> Gallery {
        Gallery { cache, dir, held: RefCell::new(Vec::new()) }
    }
}

/// What draws [`Gallery`]'s images: a figure ([`shot`]) or a Markdown image, which
/// [`content::site_url`] points at `/img/{name}`. A cheap handle: Markdown keeps one to ask for
/// each image it shows.
#[derive(Clone)]
pub(crate) struct Pictures {
    gallery: Option<Rc<Gallery>>,
    scale: f32,
}

impl Pictures {
    /// The image `docs/{name}`, drawn `pt` wide. On the web it is fetched from the site's root,
    /// whatever page this is; exporting, it is read from the output folder (the build runs
    /// there) and the build makes the exported `src` absolute.
    pub(crate) fn named(&self, name: &str, pt: f32) -> Option<Image> {
        let g = self.gallery.as_ref()?;
        #[cfg(target_arch = "wasm32")]
        return Some(g.cache.url(&format!("/img/{name}"), pt, self.scale));
        #[cfg(not(target_arch = "wasm32"))]
        return Some(g.cache.file(format!("{}/{name}", g.dir), pt, self.scale));
    }

    /// The picture for a Markdown image's source as the site resolved it (`/img/{name}`); none
    /// for any other source, which shows its alt text. The gallery keeps it from the first ask,
    /// so it loads while the page shows its alt text.
    pub(crate) fn src(&self, src: &str) -> Option<Image> {
        let name = src.strip_prefix("/img/").and_then(|n| content::IMAGES.iter().copied().find(|i| *i == n))?;
        let g = self.gallery.as_ref()?;
        if let Some((_, image)) = g.held.borrow().iter().find(|(n, _)| *n == name) {
            return Some(image.clone());
        }
        // A PNG decodes at its own size whatever the size asked for: one handle serves every width.
        let image = self.named(name, PAGE)?;
        // Natively the page is exported from its first frames, before a wake would draw the
        // picture: wait for it here (a file read and decode), so the static page carries it.
        #[cfg(not(target_arch = "wasm32"))]
        let _ = image.wait();
        g.held.borrow_mut().push((name, image.clone()));
        Some(image)
    }
}

/// The gem's angle now: a turn every `gem::TURN` seconds on the home page, still elsewhere and
/// under reduced motion.
pub(crate) fn angle(s: &State, cx: &Cx<State>) -> f32 {
    const REST: f32 = 0.35;
    if s.route != Route::Home || cx.is_reduced_motion() {
        return REST;
    }
    REST + (s.t / gem::TURN * std::f64::consts::TAU) as f32
}

/// On the home page the gem turns: about 30 frames a second of a canvas, the rest memoized.
fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    if s.route != Route::Home {
        return false;
    }
    s.t = t;
    tx.wake_after(Duration::from_millis(33));
    true
}

/// A screenshot from `docs/`, framed, holding its box at `aspect` before it arrives.
pub(crate) fn shot(s: &State, cx: &Cx<State>, name: &str, alt: &str, aspect: f32) -> El {
    let key = format!("shot-{name}");
    let shown = cx.layout_size(&key).map_or(PAGE, |sz| sz[0]);
    let mut e = el().key(key).role("image").label(alt).w_full().aspect_ratio(aspect).border(1.0, rule_faint());
    if let Some(img) = pictures(s, cx).named(name, shown) {
        e = e.pixels_fit(img.pixels(), SurfaceFit::Fit);
    }
    e
}

/// The pictures this page can draw, at the window's scale.
pub(crate) fn pictures(s: &State, cx: &Cx<State>) -> Pictures {
    s.pictures(cx.env().scale.max(1.0))
}

/// What this page's Markdown needs from the view: its pictures and the text column's width
/// (the page's frame inside its gutters), which its tables are sized in.
pub(crate) fn md_cx(s: &State, cx: &Cx<State>, bp: Bp) -> ui::MdCx {
    let column = cx.layout_size("main").map_or(PAGE, |sz| sz[0] - 2.0 * bp.gutter()).clamp(1.0, PAGE);
    ui::MdCx { pictures: pictures(s, cx), column }
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    tokens::set_dark(s.theme.dark(cx.env().system_dark));
    let bp = cx.layout_with("page", |r| Bp::of(r[2])).unwrap_or(Bp { at: 1000 });
    let gutter = bp.gutter();
    let framed = |e: El| e.w_full().max_w(PAGE + 2.0 * gutter).px(gutter);
    let mc = md_cx(s, cx, bp);
    let main = match s.route {
        Route::Home => home::page(s, cx, bp),
        Route::Benchmarks => pages::benchmarks(&mc, bp),
        Route::Debugging => pages::debugging(&mc, bp),
        Route::Limitations => pages::limitations(&mc, bp),
        Route::Docs => docs::index(bp),
        Route::Doc(doc) => docs::page(doc, &mc, bp),
        Route::Devices => devices::page(bp),
        Route::Demos => demos::page(s, cx, bp),
        Route::NotFound => not_found(bp),
    };
    col()
        .key("page")
        .w_full()
        .fill(bg())
        .items_center()
        .child(framed(ui::topbar(&s.route, s.theme, angle(s, cx))))
        .child(framed(col().key("main").role("main").child(main)))
        .child(framed(ui::footer()))
}

fn not_found(bp: Bp) -> El {
    col()
        .key("nf")
        .w_full()
        .pt(64.0)
        .gap(12.0)
        .child(rune::text("No page here.").key("nf-h").role("heading").level(1).font(bp.title()).color(fg()).wrap())
        .child(
            rune::row()
                .key("nf-links")
                .gap(18.0)
                .child(ui::link_text("nf-home", "Home", "/", BODY))
                .child(ui::link_text("nf-docs", "Docs", "/docs/", BODY)),
        )
}

pub fn spec() -> AppSpec<State> {
    // Rune sets the document's title from the spec: this page's, made once per page load.
    let title: &'static str = Box::leak(current_route().title().into_boxed_str());
    AppSpec {
        families: (SERIF, MONO),
        setup: Some(|s, waker| s.gallery = Some(Rc::new(Gallery::new(ImageCache::new(waker), "img")))),
        poll: Some(poll),
        ..AppSpec::new(title, (1000.0, 900.0), State::new, view)
    }
}

/// CMU Serif's faces, served from `/fonts/` (`public/fonts/`) and read from `fonts/` beside the
/// export, which runs in the output folder.
const FONTS: [&str; 3] = ["cmunrm.otf", "cmunti.otf", "cmunbx.otf"];

/// The site as an app: document mode, so the browser owns scrolling, find and anchors.
pub fn app() -> rune::App<State> {
    let mut app = rune::App::new(spec()).web_document();
    for face in FONTS {
        let url = if cfg!(target_arch = "wasm32") { format!("/fonts/{face}") } else { format!("fonts/{face}") };
        app = app.font_url(&url);
    }
    app
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(start))]
pub fn start() {
    #[cfg(target_arch = "wasm32")]
    if let Err(error) = app().try_run() {
        log_error(&error.to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    app().run();
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn log_error(message: &str);
}

#[cfg(test)]
mod tests;
