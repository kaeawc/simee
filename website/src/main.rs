#![forbid(unsafe_code)]

//! Natively the site runs as a window, and Rune's `--html` exports a page's static HTML for the
//! route in `SIMEE_WEB_ROUTE`. `--pages` lists every page for `scripts/build.sh`, one per line:
//! path, title and description, tab-separated. `--widths` prints the widths to export at, and
//! `--images` the images under `docs/` the pages show, which the build copies to `/img/`.

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--pages") => {
            for (path, route) in simee_website::Route::all() {
                println!("{path}\t{}\t{}", route.title(), route.description().replace(['\t', '\n'], " "));
            }
        }
        Some("--widths") => println!("{}", simee_website::EXPORT_WIDTHS),
        Some("--images") => simee_website::content::IMAGES.iter().for_each(|name| println!("{name}")),
        _ => simee_website::start(),
    }
}
