#!/usr/bin/env bash
# Encodes one loop source into every web format and prints size and SSIM for each:
# GIF (ffmpeg palettegen/paletteuse with three dithers, plus gifski and gifsicle when installed),
# MP4 (H.264, faststart, yuv420p), WebM (VP9 and AV1), animated WebP, and animated AVIF when
# avifenc is installed.
#
#   scripts/media/encode.sh SOURCE.mp4 OUT_DIR [SITE_DIR]
#     writes OUT_DIR/<variant>.<ext> and OUT_DIR/results.tsv; with SITE_DIR it also copies the chosen
#     formats there as <source name>.mp4 and .webm (H.264 and AV1, 480 px), what the website serves.
#
# SSIM is the variant against the source scaled (and, for the GIFs, frame-rate reduced) to the
# variant's own size, so it measures what the encoder lost, not what the downscale did. A tool
# that isn't installed is named and its variants skipped; nothing is installed here.
set -euo pipefail
# shellcheck source=scripts/media/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

src=${1:?usage: encode.sh SOURCE.mp4 OUT_DIR}
out=${2:?usage: encode.sh SOURCE.mp4 OUT_DIR}
site=${3:-}
name=$(basename "$src" .mp4)
need ffmpeg ffprobe || exit 1
mkdir -p "$out"
results=$out/results.tsv
printf 'variant\tfile\tbytes\tssim\tdecode_ms\n' >"$results"
for tool in gifski gifsicle avifenc img2webp; do
  command -v "$tool" >/dev/null 2>&1 || echo "media: $tool is not installed; its variants are skipped" >&2
done

w=480 # website width; the source is 660 wide
h=568

# ssim FILE FPS WIDTH HEIGHT: the file against the source scaled the same way.
ssim() {
  local f=$1 fps=$2 sw=$3 sh=$4
  ffmpeg -nostdin -i "$f" -i "$src" -lavfi \
    "[1:v]fps=$fps,scale=$sw:$sh:flags=lanczos,format=yuv420p[r];[0:v]fps=$fps,scale=$sw:$sh:flags=lanczos,format=yuv420p[d];[d][r]ssim" \
    -f null - 2>&1 | grep -o 'All:[0-9.]*' | tail -n 1 | cut -d: -f2
}

# decode_ms FILE: user CPU milliseconds ffmpeg's software decoder (one thread) takes for the whole
# loop: a proxy for decode cost, not a browser measurement (browsers decode video in hardware).
decode_ms() {
  ffmpeg -nostdin -threads 1 -benchmark -i "$1" -f null - 2>&1 | sed -n 's/.*bench: utime=\([0-9.]*\)s.*/\1/p' | tail -n 1 | awk '{printf "%d", $1 * 1000}'
}

# record NAME FILE FPS WIDTH HEIGHT
record() {
  printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$(basename "$2")" "$(stat -f%z "$2")" "$(ssim "$2" "$3" "$4" "$5")" "$(decode_ms "$2")" >>"$results"
}

gif_vf="fps=10,scale=$w:$h:flags=lanczos"
for dither in bayer:bayer_scale=5 sierra2_4a floyd_steinberg; do
  tag=${dither%%:*}
  f=$out/gif-$tag.gif
  ffmpeg -nostdin -v error -y -i "$src" \
    -vf "$gif_vf,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=$dither:diff_mode=rectangle" -loop 0 "$f"
  record "GIF palettegen, $tag dither, 10 fps" "$f" 10 $w $h
  if command -v gifsicle >/dev/null 2>&1; then
    gifsicle -O3 --lossy=80 "$f" -o "$out/gif-$tag-lossy.gif"
    record "GIF palettegen $tag + gifsicle -O3 --lossy=80" "$out/gif-$tag-lossy.gif" 10 $w $h
  fi
done

if command -v gifski >/dev/null 2>&1; then
  mkdir -p "$out/frames"
  ffmpeg -nostdin -v error -y -i "$src" -vf "$gif_vf" "$out/frames/%04d.png"
  gifski --quality 80 --fps 10 -o "$out/gifski.gif" "$out"/frames/*.png
  record "gifski quality 80, 10 fps" "$out/gifski.gif" 10 $w $h
fi

ffmpeg -nostdin -v error -y -i "$src" -vf "scale=660:780:flags=lanczos" -c:v libx264 -crf 28 -preset slow \
  -pix_fmt yuv420p -movflags +faststart -an "$out/mp4-660.mp4"
record "MP4 H.264 CRF 28, 660 px, 30 fps" "$out/mp4-660.mp4" 30 660 780
ffmpeg -nostdin -v error -y -i "$src" -vf "scale=$w:$h:flags=lanczos" -c:v libx264 -crf 28 -preset slow \
  -pix_fmt yuv420p -movflags +faststart -an "$out/mp4-480.mp4"
record "MP4 H.264 CRF 28, 480 px, 30 fps" "$out/mp4-480.mp4" 30 $w $h
ffmpeg -nostdin -v error -y -i "$src" -vf "scale=$w:$h:flags=lanczos" -c:v libvpx-vp9 -crf 36 -b:v 0 -row-mt 1 -an "$out/webm-vp9.webm"
record "WebM VP9 CRF 36, 480 px, 30 fps" "$out/webm-vp9.webm" 30 $w $h
ffmpeg -nostdin -v error -y -i "$src" -vf "scale=$w:$h:flags=lanczos" -c:v libsvtav1 -crf 40 -preset 6 -an "$out/webm-av1.webm"
record "WebM AV1 (SVT) CRF 40, 480 px, 30 fps" "$out/webm-av1.webm" 30 $w $h

# Animated WebP (lossy, 15 fps) from frames: ffmpeg has no libwebp encoder here, img2webp does.
if command -v img2webp >/dev/null 2>&1; then
  mkdir -p "$out/webp-frames"
  ffmpeg -nostdin -v error -y -i "$src" -vf "fps=15,scale=$w:$h:flags=lanczos" "$out/webp-frames/%04d.png"
  img2webp -loop 0 -d 67 -lossy -q 70 -m 6 "$out"/webp-frames/*.png -o "$out/anim.webp"
  record "Animated WebP lossy q70, 480 px, 15 fps" "$out/anim.webp" 15 $w $h
fi

# Animated AVIF needs avifenc: ffmpeg's avif muxer writes the first frame only (one frame in the
# file, checked with ffprobe -count_frames), so it is not a fallback.
if command -v avifenc >/dev/null 2>&1; then
  avifenc -q 50 --fps 15 "$out"/webp-frames/*.png "$out/anim.avif" >/dev/null
  record "Animated AVIF avifenc q50, 480 px, 15 fps" "$out/anim.avif" 15 $w $h
else
  echo "media: animated AVIF not encoded (avifenc is not installed)" >&2
fi

if [[ -n $site ]]; then
  mkdir -p "$site"
  cp "$out/mp4-480.mp4" "$site/$name.mp4"
  cp "$out/webm-av1.webm" "$site/$name.webm"
fi

column -t -s "$(printf '\t')" "$results"
