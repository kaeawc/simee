#!/usr/bin/env bash
# Builds the site into website/dist: the app to wasm (Rune's scripts/web-build.sh), then every
# page's static HTML, written by the app itself (`--html`) into index.html, plus the images,
# robots.txt, sitemap.xml and llms.txt. Serve dist/ with any static host; the Dockerfile serves
# it on Fly.
#
#   website/scripts/build.sh                 # production: https://simee.fly.dev
#   website/scripts/build.sh --dev           # quicker, larger wasm; noindex everywhere
#   SIMEE_WEB_ORIGIN=https://simee.dev website/scripts/build.sh
#
# Needs the wasm32-unknown-unknown target, the wasm-bindgen CLI at the version in Cargo.lock and
# python3. binaryen's wasm-opt, when installed, makes the wasm smaller.
set -euo pipefail
site=$(cd "$(dirname "$0")/.." && pwd)
repo=$(cd "$site/.." && pwd)
out=${OUT:-$site/dist}
origin=${SIMEE_WEB_ORIGIN:-https://simee.fly.dev}
# The live demo's relay (website/media), which /demos/ plays from.
media=${SIMEE_WEB_MEDIA_ORIGIN:-https://simee-media.fly.dev}
dev=false
for a in "$@"; do
  case $a in
    --dev) dev=true ;;
    *)
      echo "usage: website/scripts/build.sh [--dev]" >&2
      exit 2
      ;;
  esac
done
cd "$site"

# Rune's web build script, from the checkout cargo already has at the pinned rev.
manifest=$(cargo metadata --format-version 1 --locked | python3 -c '
import json, sys
m = json.load(sys.stdin)
print(next(p["manifest_path"] for p in m["packages"] if p["name"] == "rune"))')
rune=$(cd "$(dirname "$manifest")/../.." && pwd)

rm -rf "$out"
mkdir -p "$out/app" "$out/img"

# The wasm: smallest with whole-program LTO. Rune's Markdown wants a 2 MiB wasm stack.
web=(--out "$out/app" --no-html)
if $dev; then
  web+=(--dev)
else
  export CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_LTO=fat
  command -v wasm-opt >/dev/null && web+=(--opt)
fi
RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-zstack-size=2097152" bash "$rune/scripts/web-build.sh" simee-website "${web[@]}"

cp -R public/. "$out/"

# The static pages: the native app lays each route out at every width and fills the template,
# in the light theme (index.html's CSS inverts it for dark).
cargo build --locked -q --bin simee-website
bin=$site/target/debug/simee-website
widths=$("$bin" --widths)
# Every image a page shows (content::IMAGES), before the export, which reads them from img/.
while IFS= read -r image; do
  cp "$repo/docs/$image" "$out/img/"
done < <("$bin" --images)
page() { # page PATH TITLE DESCRIPTION FILE CANONICAL
  local path=$1 title=$2 description=$3 file=$4 canonical=$5
  mkdir -p "$(dirname "$file")"
  python3 - "$site/index.html" "$file" "$origin" "$path" "$title" "$description" "$canonical" "$dev" "$media" <<'PY'
import html, sys
template, file, origin, path, title, description, canonical, dev, media = sys.argv[1:]
url = origin + path
page = open(template).read()
page = page.replace("<!--robots-->", '<meta name="robots" content="noindex, nofollow" />' if dev == "true" else "")
page = page.replace("<!--canonical-->", f'<link rel="canonical" href="{html.escape(url)}" />' if canonical == "1" else "")
for key, value in (("__TITLE__", title), ("__DESCRIPTION__", description), ("__URL__", url), ("__ORIGIN__", origin), ("__MEDIA__", media)):
    page = page.replace(key, html.escape(value))
open(file, "w").write(page)
PY
  (cd "$out" && SIMEE_WEB_ROUTE=$path "$bin" --html "$file" --html-widths "$widths" --html-template "$file" --theme light)
  # Exported images name the file the app read, relative to dist/; every page needs it from the root.
  python3 - "$file" <<'PY'
import sys
f = sys.argv[1]
s = open(f).read().replace('src="img/', 'src="/img/')
open(f, "w").write(s)
PY
}

pages=$("$bin" --pages)
paths=()
while IFS=$'\t' read -r path title description; do
  page "$path" "$title" "$description" "$out${path}index.html" 1
  paths+=("$path")
done <<<"$pages"
page /404 "Not found · simee" "There's no page here." "$out/404.html" 0

if $dev; then
  printf 'User-agent: *\nDisallow: /\n' >"$out/robots.txt"
else
  printf 'User-agent: *\nAllow: /\n\nSitemap: %s/sitemap.xml\n' "$origin" >"$out/robots.txt"
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">'
    for path in "${paths[@]}"; do echo "  <url><loc>$origin$path</loc></url>"; done
    echo '</urlset>'
  } >"$out/sitemap.xml"
fi

# llms.txt (llmstxt.org): what simee is and where each doc's Markdown is.
{
  echo "# simee"
  echo
  echo "> $(head -1 <<<"$pages" | cut -f3)"
  echo
  echo "## Docs"
  echo
  while IFS=$'\t' read -r path title _; do
    case $path in
      /docs/?*) echo "- [${title% · simee docs}](https://raw.githubusercontent.com/kaeawc/simee/main${path%/}.md)" ;;
      *) ;;
    esac
  done <<<"$pages"
  echo
  echo "## Source"
  echo
  echo "- [kaeawc/simee](https://github.com/kaeawc/simee): the code, MIT"
} >"$out/llms.txt"

echo "Built $(find "$out" -name index.html | wc -l | tr -d ' ') pages into ${out#"$repo"/}/ for $origin"
