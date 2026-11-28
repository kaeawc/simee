# simee website

simee's public site, written in Rune. Every page is one Rune app (`src/`, package `simee-website`)
in web document mode: the browser scrolls a semantic mirror of it (headings, paragraphs, real
links, landmarks), and the canvas draws the part in view. The same app writes each page's static
HTML with `--html`, which is the first paint and what crawlers and readers without WebGPU or
JavaScript get. It is its own Cargo workspace, like `desktop/`, pinned to the same Rune rev.

## What it shows

Set as a paper (CMU Serif, numbered sections, captioned tables, booktabs rules; design rules from
impeccable.style), with no copy that repeats the README or a doc. Paragraphs stop at the text
column (680 pt); tables and figures use half again that width where the browser has it.

- `/`: title, Figure 1 (the mark, a gem Rune draws and turns, `src/gem.rs`), the abstract (the
  README's first paragraph), §1 Performance (the README's one table, iOS and Android), §2 Quick
  start (the README's, without its support table) and further reading.
- `/benchmarks/`: the README's comparison with the stock tools, a table per platform.
- `/debugging/`: `docs/debugging.md`'s iOS debug surface.
- `/limitations/`: the README's "Not yet".
- `/docs/` and `/docs/{slug}/`: the Markdown in `docs/`, the list in `src/content.rs` (`DOCS`).
  Links to another published doc become its page, `docs/*.png` become `/img/` (an image a doc
  shows is listed in `IMAGES`, which the build copies), anything else in the repository its
  GitHub page, and so does a code span that is a repository path
  (`scripts/bench/android-boot.sh`). Headings keep GitHub's anchors, but one that starts with a
  digit gets `h-` (`#h-3d-device-view`): Rune's export drops such an id. Wide tables scroll
  sideways on a phone.
- `/devices/`: device concepts from `sim-core`'s support matrix and reference profiles, so the
  page changes when `form_factor.rs` does.
- `/demos/`: the desktop window, as figures. The live demo (emu and the SDK's emulator side by
  side) is built but hidden (`LIVE` in `src/demos.rs`) until its relay streams:
  `public/live.js` plays it over WHEP from `media/` (its README), fed by `scripts/live-demo.sh`.
  Build with `SIMEE_WEB_MEDIA_ORIGIN` to play from another relay.

Every number on the site is one the README or a doc quotes from `docs/benchmarks.md`; the site
adds captions only.

## Light and dark

The page follows the system's `prefers-color-scheme`, and the toggle at the end of the running
head cycles System, Light, Dark. The choice is kept in `localStorage` (`simee-theme`) and set on
`<html data-theme>` before the first paint (`src/theme.rs`, `index.html`). Rune draws both themes
(`tokens.rs`). The static HTML is exported in the light theme; in dark, `index.html`'s CSS inverts
it and turns pictures and the gem back, so readers without WebGPU get a dark page too and Rune's
first frame (black page) matches it.

Fonts: CMU Serif (Computer Modern Unicode, SIL OFL, `public/fonts/CMU-OFL.txt`) for text, Rune's
embedded IBM Plex Mono for code.

## Build and preview

Needs the `wasm32-unknown-unknown` target and the wasm-bindgen CLI at the version in
`Cargo.lock` (`cargo install wasm-bindgen-cli --version <it> --locked`); binaryen's `wasm-opt`,
when installed, makes the wasm smaller.

```bash
website/scripts/build.sh --dev                  # quicker, larger wasm; noindex
website/scripts/build.sh                        # production, into website/dist
python3 -m http.server 8765 -d website/dist     # http://localhost:8765
cd website && cargo test                        # routes, links, anchors, every page at every width
```

WebGPU works over plain HTTP on `localhost`. The static page shows first and stays for browsers
without WebGPU. Each page lays out the same tree at every width (a test checks it), because the
exported HTML switches only styles with `@media`.

## Hosting and deploys

Fly app `simee`, served at https://simee.fly.dev: Caddy serving `dist/` (`Dockerfile`,
`Caddyfile`, `fly.toml`). `.github/workflows/website.yml` builds and tests on every pull request
that touches what the site reads, and on a push to main deploys with `flyctl deploy` using the
`FLY_API_TOKEN` secret of the `website` environment. Nothing builds on Fly; the image is Caddy
plus the files Actions built.

One-time setup, by someone with access to the Fly organization and the repository:

```bash
fly apps create simee
fly tokens create deploy -a simee -x 8760h | gh secret set FLY_API_TOKEN --env website -R kaeawc/simee
```

A deploy token is scoped to that one app, so the secret can't touch anything else in the Fly
organization. Rotate it by creating a new one and setting the secret again. Until the secret
exists, a push to main builds the site and the deploy job passes with a notice that it deployed
nothing. To deploy without waiting for a push, run the workflow by hand (Actions, Website, Run
workflow) or `cd website && scripts/build.sh && fly deploy --remote-only`.

To serve a custom domain: `fly certs add <domain> -a simee`, point the domain's DNS at the
app as `fly certs show` says, then set the repository variable `SIMEE_WEB_ORIGIN` to
`https://<domain>` so canonical URLs, the sitemap and social cards name it.

Check an image before deploying: `docker build -t simee website && docker run --rm -p 8080:8080
simee`, then http://localhost:8080 (WebGPU works on localhost).
