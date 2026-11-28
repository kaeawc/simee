# 3D device-view media: stills, loops, and live Rune against recorded video

Research for [#584](https://github.com/kaeawc/simee/issues/584): stills of the desktop's
[3D device view](../desktop.md#3d-device-view) for every form factor, scripted 15 s loops, and a
comparison of ways to show the motion on simee.dev. Run on an Apple M3 Max (Mac15,9), macOS 26.6,
2026-11-28, while the host's 1-minute load was 16 to 43, so none of the timings below is a
quiet-host number ([bench-protocol.md](bench-protocol.md)). Sizes and SSIM do not depend on load.

Nothing here is a `docs/benchmarks.md` row: the loops are not a product number, and the one live
timing is a single run.

## What `scripts/media/` does

| Script | Output |
| --- | --- |
| `scripts/media/stills.sh` | 15 form factors x 4 camera angles (3/4, front, side, back) = 60 stills in `website/assets/stills/` |
| `scripts/media/loops.sh` | the 15 s sources: `switch`, `tour` (headless), `boot` (a real window, needs a boot slot) |
| `scripts/media/encode.sh` | every format below from one source, with size, SSIM and decode time; copies the chosen two into `website/assets/loops/` |

Stills: `simee-desktop --snap` with `SIMEE_VIEW=3d` and `SIMEE_SCENE_CAMERA`, cropped to the 3D
stage (`--crop scene-3d`), 1.5x scale, then `cwebp -q 90`. The 15 scenes are the smoke lane's
form-factor scenes: iPhone, iPad, Apple Watch, Apple TV, CarPlay head unit, Vision Pro, Android
phone, foldable open and folded, tablet, Wear OS round, Android TV, Automotive head unit, XR
headset and XR glasses. Every one has a 3D body. The 60 WebPs total 636 KB (about 10.6 KB each);
the same renders as PNG are 2.5 MB. `oxipng` is not installed, so the PNGs are not
optimized (the script skips that step and says so); only the WebPs are committed.

Loops: `switch` is phone, tablet, watch, phone (3.75 s each); `tour` is iPhone, iPad, Apple
Watch, open foldable, iPhone (3 s each). Each body turns about 180 degrees to its back and returns
(a scripted orbit drag out and the same drag back), so the last frame is the first: the seam SSIM
is 0.992 for both. The desktop renders them headless (`--script` and `--video`, synthetic
screens), so no window is on the screen and no device is involved.

## What was not recorded

- **Boot and shutdown loop: not recorded.** It needs the real window, a booted lane AVD and a
  boot slot. The host was at `HOLD` (load 43 against a limit of 24), `target/release/emu` is not
  built in this worktree and no `lane-media-*` AVD exists. `loops.sh boot` is written (the window
  runs under `boot-slot.sh`, `--window-script` clicks the shut-down device and then the toolbar's
  Shutdown, frames come from `screencapture -l` of that process's window id) but it has not been
  run, and it refuses to start a window unless the AVD, the signed `emu` and the slot all exist.
  To record it: build and sign `emu`, `emu -create-avd lane-media-1 -no-golden ...`, then
  `scripts/media/loops.sh boot`.
- **Switch and tour are one body per run.** A headless scene is one device, and a headless
  `Select` changes the selection but not the synthetic screen, so the loops join per-body runs
  rather than clicking through the device list. The real list-driven switch needs live devices.
- **gifski, gifsicle and avifenc are not installed** (nothing was installed). Their GIF
  variants and animated AVIF are not measured. ffmpeg's `avif` muxer writes only the first frame
  (checked: 1 frame in the file), so it is not a substitute.
- **Decode CPU in a browser, battery, mobile**: not measured. `decode ms` below is ffmpeg's
  single-thread software decode time of the whole 15 s loop, a proxy only; browsers decode H.264
  and VP9 in hardware, and GIF and WebP in software.

## Format comparison (measured)

Source: the 15 s loop at 660x780, 30 fps, H.264 CRF 8. SSIM is each file against the source scaled
(and, for GIF and WebP, frame-rate reduced) to the file's own size. The two loops agree; `tour`:

| Variant | Size | SSIM | Decode, ms (ffmpeg sw, 1 thread) |
| --- | --- | --- | --- |
| GIF, palettegen + bayer dither, 480 px, 10 fps | 3,066,162 B | 0.982 | 93 |
| GIF, palettegen + sierra2_4a, 480 px, 10 fps | 4,909,458 B | 0.972 | 110 |
| GIF, palettegen + floyd_steinberg, 480 px, 10 fps | 4,668,425 B | 0.974 | 105 |
| Animated WebP, lossy q70, 480 px, 15 fps | 468,076 B | 0.963 | 153 |
| MP4 H.264 CRF 28, `+faststart`, yuv420p, 660 px, 30 fps | 308,724 B | 0.996 | 211 |
| MP4 H.264 CRF 28, 480 px, 30 fps | 202,665 B | 0.995 | 115 |
| WebM VP9 CRF 36, 480 px, 30 fps | 200,037 B | 0.997 | 119 |
| WebM AV1 (SVT-AV1) CRF 40, 480 px, 30 fps | 149,573 B | 0.997 | 147 |

`switch` (same order): 3,013,324 B / 0.981; 4,559,462 B / 0.976; 4,342,021 B / 0.977;
485,542 B / 0.965; 300,246 B / 0.997; 195,814 B / 0.996; 186,285 B / 0.997; 140,922 B / 0.997.

The video formats are 15 to 30 times smaller than the GIFs at a higher SSIM, and 30 fps against
10. A GIF is limited to 256 colours, and these scenes are smooth gradients, so the GIF's dither
(bayer scored best) is the quality ceiling. Not run: gifski, gifsicle `-O3 --lossy`, animated
AVIF.

## Live Rune WASM against recorded media

Measured from a local production build of `website/` (`website/scripts/build.sh`, 26 pages, no
`wasm-opt` installed, served by `python3 -m http.server`, one run in the app's browser pane):

| | Value |
| --- | --- |
| `app_bg.wasm` | 10,197,250 B raw, 3,664,799 B gzip |
| `app.js` and Rune's JS modules | about 0.4 MB |
| CMU Serif and Inter fonts | about 1.8 MB (cached after the first load) |
| Static HTML first paint (page `/`) | 144 ms |
| `web.first.presented` (Rune's first canvas frame) | 324 ms |
| A 15 s loop, MP4 or WebM, 480 px | 141 to 203 KB |

The first-frame figure is localhost, with the wasm already on disk: it includes compile and
first draw, and no network. The site as built does not draw the 3D device view: the 3D scene is
the desktop's compositor, and nothing of it is compiled into `simee-website`. So there is no
measured "live 3D" row; what is measured is the cost the site already pays to be a Rune app, and
a live 3D scene would add to it.

Estimates (not measurements):

- Over a 10 Mbit/s link the existing wasm is about 2.9 s to download and a loop about 0.12 s;
  at 100 Mbit/s, 0.3 s and 0.01 s. A live scene adds its shaders and bodies to the wasm.
- A looping `<video muted>` decodes in hardware at a few percent of a core; a live scene draws
  continuously on the GPU (the desktop's 3D view costs 0.4 ms CPU and 1.8 ms GPU a frame at
  60 fps, [desktop.md](../desktop.md#3d-device-view), headless on this class of GPU), so battery
  on a phone is expected to favour the video. Not measured.

| | Live Rune WASM | MP4 / WebM loop | Animated WebP / GIF |
| --- | --- | --- | --- |
| Page weight | wasm already paid for the site; 3D adds to it (unmeasured) | 141 to 203 KB (measured) | 0.47 MB (WebP), 3 to 5 MB (GIF) (measured) |
| Time to first frame | after wasm and GPU init; 324 ms local for the page's canvas, seconds on a slow link (estimate) | poster at once, first frame after the first KB of a faststart file | immediate, whole file first |
| Sharpness at hi-DPI | exact at any size | fixed at 480 or 660 px: soft on 2x and 3x screens | fixed at 480 px |
| Interaction | orbit with the pointer | none | none |
| Needs WebGPU | yes (the site's static HTML is the fallback) | no | no |
| Reduced motion, pause | the app decides | `autoplay` can be dropped and the `<video>` paused from `prefers-reduced-motion` | a GIF cannot be paused; WebP only by replacing it with a still |

## Recommendation (per use)

- **README and social: MP4.** GitHub renders `<video>` from an attached MP4, and social cards
  take MP4. 200 to 300 KB for 15 s. A GIF only where video is not accepted, at 480 px, 10 fps,
  bayer dither, and expect 3 MB.
- **Docs, inline: WebM AV1 with the MP4 as the fallback source**, in
  `<video autoplay muted loop playsinline preload="metadata" poster=...>`, with the 3/4 still
  (about 10 KB WebP) as the poster. 141 to 150 KB per loop. Both are committed in
  `website/assets/loops/`. Under `prefers-reduced-motion: reduce`, drop `autoplay` and show the
  poster with a play control.
- **simee.dev hero: the loop first, live Rune later.** The loop costs under 0.2 MB and shows the
  3D view on every browser. Live Rune is the only option that stays sharp at hi-DPI and that
  can be orbited, but it needs a 3D scene compiled into the site's wasm and was not
  measured here; do that as its own change, then measure it against the loop (battery included),
  with the loop as the no-WebGPU fallback.

## Wiring and gaps

The site is a Rune app, so a `<video>` needs an element the Rune page can embed (the live demo's
`public/live.js` does it for `/demos/`); this note does not wire the assets into a page, and
`prefers-reduced-motion` handling is therefore not implemented. Rune limitations hit: none new.
The stills and loops are regenerated from a clean checkout with
`scripts/media/stills.sh` and `scripts/media/loops.sh`, then
`scripts/media/encode.sh target/media/src/<name>.mp4 target/media/<name> website/assets/loops`.
