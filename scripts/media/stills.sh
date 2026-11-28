#!/usr/bin/env bash
# 3D device-view stills of every form factor at four camera angles (3/4, front, side, back):
# the desktop's headless render (`--snap`, `SIMEE_VIEW=3d`, `SIMEE_SCENE_CAMERA`) cropped to the
# 3D stage, then optimized (oxipng for PNG and cwebp for WebP, each when installed).
#
#   scripts/media/stills.sh [OUT_DIR]     # default website/assets/stills
#
# SIMEE_STILLS_SCALE (default 1.5) is the device scale. SIMEE_STILLS_PNG=1 keeps the PNGs too.
set -euo pipefail
# shellcheck source=scripts/media/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

out=${1:-$root/website/assets/stills}
scale=${SIMEE_STILLS_SCALE:-1.5}
build_desktop
mkdir -p "$out"

for tool in oxipng cwebp; do
  command -v "$tool" >/dev/null 2>&1 || echo "media: $tool is not installed; that optimization is skipped" >&2
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
count=0
for entry in "${scenes[@]}"; do
  scene=${entry%%:*}
  name=${entry#*:}
  for angle in "${angles[@]}"; do
    label=${angle%%:*}
    camera=${angle#*:}
    png=$work/$name-$label.png
    SIMEE_VIEW=3d SIMEE_SCENE_CAMERA=$camera "$desktop_bin" --snap "$png" --scene "$scene" \
      --scale "$scale" --crop scene-3d >/dev/null 2>"$work/err" || {
      cat "$work/err" >&2
      exit 1
    }
    if command -v oxipng >/dev/null 2>&1; then oxipng -q -o 4 --strip safe "$png"; fi
    if command -v cwebp >/dev/null 2>&1; then
      cwebp -quiet -q 90 -m 6 "$png" -o "$out/$name-$label.webp"
    fi
    # PNGs only without cwebp, or on request (SIMEE_STILLS_PNG=1): the WebPs are a quarter the size.
    if [[ -n ${SIMEE_STILLS_PNG:-} ]] || ! command -v cwebp >/dev/null 2>&1; then
      cp "$png" "$out/$name-$label.png"
    fi
    count=$((count + 1))
  done
done
echo "media: $count stills in $out"
