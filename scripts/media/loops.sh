#!/usr/bin/env bash
# The three scripted 15 s loops, written as lossless-ish sources (H.264 CRF 8, 30 fps) that
# scripts/media/encode.sh turns into the website formats.
#
#   scripts/media/loops.sh [switch|tour|boot|all] [OUT_DIR]     # default all, target/media/src
#
# switch  phone, tablet, watch, phone: four 3.75 s bodies, each swept through its back and home.
# tour    five 3 s bodies (iPhone, iPad, Apple Watch, open foldable, iPhone again), one sweep each.
# boot    a shut-down lane AVD booted from the device list, then the toolbar's Shutdown, in the real
#         window (`--window-script`, frames by window id with `screencapture -l`). It needs a boot
#         slot and the lane-owned AVD lane-media-1 (`SIMEE_MEDIA_AVD`, port 5740), and exits 75
#         when no slot comes: that loop is then not recorded.
#
# switch and tour use the desktop's headless scenes (synthetic screens, `--script` and `--video`):
# deterministic, and nothing on screen is a real device or the owner's desktop. A scene is one
# device, so each body is its own run and the runs are joined; a headless Select does not swap the
# synthetic screen. The drag back starts beside the body (a drag on the drawn screen belongs to
# the device) and not before the first drag's release. A sweep is a drag that turns the body about 180 degrees and the same drag
# back, so the last frame is the first (SSIM >= 0.99 against it, checked below).
set -euo pipefail
# shellcheck source=scripts/media/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

what=${1:-all}
out=${2:-$root/target/media/src}
need ffmpeg ffprobe python3 || exit 1
mkdir -p "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The stage inside the window at --scale 2: x, y, width, height in pixels.
stage_crop="1320:1560:480:120"
lead=0.6 # the 3D view settles into its rest pose in the first frames: rendered, then trimmed
fps=30

# segment SCENE SECONDS OUT.mp4: one body for SECONDS, swept out and back.
segment() {
  local scene=$1 secs=$2 dest=$3 half
  half=$(python3 -c "print(round(($secs - 0.6) / 2, 3))")
  python3 - "$half" "$secs" "$lead" >"$work/script.json" <<'PY'
import json, sys
half, secs, lead = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3])
out = {"t": 0.3 + lead, "type": "drag", "x": 300, "y": 500, "dx": 320, "dur": half}
back = {"t": 0.4 + lead + half, "type": "drag", "x": 840, "y": 500, "dx": -320, "dur": half}
print(json.dumps([out, back, {"t": secs + lead, "type": "mark", "name": "end"}]))
PY
  SIMEE_VIEW=3d "$desktop_bin" --scene "$scene" --script "$work/script.json" --scale 2 --fps "$fps" \
    --duration "$(python3 -c "print($secs + $lead)")" --video "$work/raw.mp4" >/dev/null 2>"$work/err" || {
    cat "$work/err" >&2
    exit 1
  }
  ffmpeg -v error -y -ss "$lead" -i "$work/raw.mp4" -t "$secs" -vf "crop=$stage_crop,scale=660:780:flags=lanczos" \
    -c:v libx264 -crf 8 -preset slow -pix_fmt yuv420p -r "$fps" "$dest"
}

# join SOURCE.mp4...: concatenates equal-format segments into $1, exactly 15 s.
join() {
  local dest=$1 list=$work/list.txt
  shift
  : >"$list"
  for f in "$@"; do echo "file '$f'" >>"$list"; done
  ffmpeg -v error -y -f concat -safe 0 -i "$list" -c copy -frames:v 450 "$dest"
}

# closes FILE: first and last frame's SSIM, the loop's seam.
closes() {
  ffmpeg -v error -y -i "$1" -vf "select=eq(n\,0)" -frames:v 1 "$work/first.png"
  ffmpeg -v error -y -sseof -0.1 -i "$1" -frames:v 1 -update 1 "$work/last.png"
  ffmpeg -i "$work/first.png" -i "$work/last.png" -lavfi ssim -f null - 2>&1 | grep -o 'All:[0-9.]*'
}

record_headless() {
  local name=$1 dur=$2 i=0
  shift 2
  local parts=()
  for entry in "$@"; do
    i=$((i + 1))
    segment "${entry%%:*}" "${entry#*:}" "$work/$name-$i.mp4"
    parts+=("$work/$name-$i.mp4")
  done
  join "$out/$name.mp4" "${parts[@]}"
  echo "media: $name $(ffprobe -v error -show_entries format=duration -of csv=p=0 "$out/$name.mp4") s (wanted $dur), seam SSIM $(closes "$out/$name.mp4")"
}

# window_frames SECONDS DIR PID: frames of the lane's own desktop window (the one process PID owns)
# by window id, never the screen.
window_frames() {
  local secs=$1 dir=$2 pid=$3 id i=0 deadline
  mkdir -p "$dir"
  id=$(
    python3 - "$pid" <<'PY'
import sys, Quartz
for w in Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionAll, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") == int(sys.argv[1]) and w.get("kCGWindowLayer") == 0:
        print(w["kCGWindowNumber"]); break
PY
  )
  [[ -n $id ]] || {
    echo "media: no simee-desktop window to capture" >&2
    return 1
  }
  deadline=$((SECONDS + secs))
  while ((SECONDS < deadline)); do
    screencapture -x -o -l "$id" "$dir/$(printf '%05d' "$i").png"
    i=$((i + 1))
  done
}

export -f window_frames

record_boot() {
  local avd=${SIMEE_MEDIA_AVD:-lane-media-1} slot emu=$root/target/release/emu
  # Preflight, before any window opens: a lane-owned AVD, the signed release emu, a boot slot.
  [[ $avd == lane-media-* ]] || {
    echo "media: $avd is not a lane-media AVD" >&2
    return 1
  }
  [[ -x $emu ]] || {
    echo "media: $emu is not built; boot loop not recorded" >&2
    return 1
  }
  [[ -d $HOME/.android/avd/$avd.avd ]] || {
    echo "media: AVD $avd does not exist; boot loop not recorded" >&2
    return 1
  }
  slot=$(find /Users/jason/.claude/plugins/marketplaces/waaagh/skills/orchestrate/scripts -name boot-slot.sh | head -n 1)
  [[ -x $slot ]] || {
    echo "media: boot-slot.sh not found; boot loop not recorded" >&2
    return 75
  }
  need screencapture || return 1
  python3 - "$avd" >"$work/boot.json" <<'PY'
import json, sys
print(json.dumps([
  {"t": 1.0, "type": "click", "key": "device-" + sys.argv[1]},
  {"t": 9.0, "type": "click", "key": "shutdown-button"},
  {"t": 15.0, "type": "mark", "name": "end"}]))
PY
  # The window's click boots the AVD: the whole run holds the boot slot. A window script ends its
  # replay, not the window, so the window is stopped (SIGINT) once its 15 s are captured.
  # shellcheck disable=SC2016 # the inner script's variables expand in the inner shell
  SIMEE_DEVICE=$avd SIMEE_VIEW=3d "$slot" --label media-boot -- bash -c '
    "$1" --window-script "$2" >"$3" 2>&1 &
    pid=$!
    sleep 3
    window_frames 15 "$4" "$pid"
    kill -INT "$pid"' _ "$desktop_bin" "$work/boot.json" "$work/window.log" "$work/boot-frames" || return $?
  ffmpeg -v error -y -framerate 10 -i "$work/boot-frames/%05d.png" -vf "crop=$stage_crop,scale=660:780" \
    -c:v libx264 -crf 8 -pix_fmt yuv420p "$out/boot.mp4"
}

build_desktop
case $what in
  switch | tour | boot | all) ;;
  *)
    echo "usage: loops.sh [switch|tour|boot|all] [OUT_DIR]" >&2
    exit 2
    ;;
esac
if [[ $what == switch || $what == all ]]; then
  record_headless switch 15 android:3.75 tablet:3.75 watch:3.75 android:3.75
fi
if [[ $what == tour || $what == all ]]; then
  record_headless tour 15 demo:3 ipad:3 apple-watch:3 foldable:3 demo:3
fi
if [[ $what == boot || $what == all ]]; then
  record_boot || echo "media: boot loop not recorded (exit $?)" >&2
fi
