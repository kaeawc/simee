#!/usr/bin/env bash
# The website's live demo: streams running Android devices to the demo relay (website/media) so
# https://simee.fly.dev/demos/ plays them. One device under emu and one under the SDK's
# emulator, the same AVD, give the side-by-side; either alone works.
#
#   SIMEE_MEDIA_PASS=... scripts/live-demo.sh --emu emulator-5554 --sdk emulator-5556
#   scripts/live-demo.sh --emu emulator-5554 --media http://127.0.0.1:8889 --no-drive   # local relay
#
# Each device's screen comes from its own encoder (`adb exec-out screenrecord --output-format=h264`),
# so both are encoded the same way, and goes to the relay over WHIP with ffmpeg (9.0 or newer: its
# `whip` muxer). screenrecord stops after its time limit; the loop starts it again into the same
# WHIP session, so viewers stay connected. With --drive (the default) both devices get the same
# input every few seconds: the app drawer, Settings, a scroll, home.
#
# Needs adb, ffmpeg and, for a relay that isn't on loopback, SIMEE_MEDIA_PASS (the relay's
# publisher password). Stop with Ctrl-C: it stops only the processes it started.
set -euo pipefail

usage="usage: scripts/live-demo.sh [--emu SERIAL] [--sdk SERIAL] [--media URL] [--bitrate BPS] [--no-drive]"
media=${SIMEE_MEDIA_ORIGIN:-https://simee-media.fly.dev}
bitrate=4000000
drive=true
declare -a names=() serials=()
while (($#)); do
  case $1 in
    --emu | --sdk)
      names+=("${1#--}")
      serials+=("${2:?$1 needs a serial}")
      shift 2
      ;;
    --media)
      media=${2:?--media needs a URL}
      shift 2
      ;;
    --bitrate)
      bitrate=${2:?--bitrate needs bits per second}
      shift 2
      ;;
    --no-drive)
      drive=false
      shift
      ;;
    -h | --help)
      echo "$usage"
      exit 0
      ;;
    *)
      echo "$usage" >&2
      exit 2
      ;;
  esac
done
((${#names[@]})) || {
  echo "$usage" >&2
  exit 2
}
for tool in adb ffmpeg; do
  command -v "$tool" >/dev/null || {
    echo "live-demo: $tool isn't on PATH" >&2
    exit 3
  }
done
ffmpeg -hide_banner -muxers 2>/dev/null | grep -q ' whip ' || {
  echo "live-demo: this ffmpeg has no whip muxer (ffmpeg 9.0 or newer has it)" >&2
  exit 3
}

# The relay's WHIP URL for a stream, with the publisher's credentials (HTTP basic auth).
whip_url() {
  local scheme=${media%%://*} rest=${media#*://}
  if [[ -n ${SIMEE_MEDIA_PASS:-} ]]; then
    printf '%s://publisher:%s@%s/%s/whip' "$scheme" "$SIMEE_MEDIA_PASS" "$rest" "$1"
  else
    printf '%s/%s/whip' "$media" "$1"
  fi
}

pids=()
cleanup() {
  for pid in "${pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# One device: screenrecord in a loop (it stops at its time limit) into one ffmpeg WHIP session.
# Raw H.264 has no timestamps; the wall clock gives each frame its time.
stream() {
  local name=$1 serial=$2
  adb -s "$serial" get-state >/dev/null
  while true; do
    adb -s "$serial" exec-out screenrecord --output-format=h264 --bit-rate "$bitrate" - || sleep 1
  done | ffmpeg -hide_banner -loglevel warning -use_wallclock_as_timestamps 1 -f h264 -i - \
    -c:v copy -f whip "$(whip_url "$name")"
}

# The same input on every device: positions are fractions of each screen.
tap_all() { # verb args as fractions
  local i serial size w h
  for i in "${!serials[@]}"; do
    serial=${serials[$i]}
    size=$(adb -s "$serial" shell wm size | awk -F'[ x]' '/Physical/ {print $3 "x" $4}')
    w=${size%x*} h=${size#*x}
    case $1 in
      swipe) adb -s "$serial" shell input swipe "$(awk "BEGIN{print int($w*$2)}")" "$(awk "BEGIN{print int($h*$3)}")" \
        "$(awk "BEGIN{print int($w*$4)}")" "$(awk "BEGIN{print int($h*$5)}")" 350 & ;;
      settings) adb -s "$serial" shell am start -a android.settings.SETTINGS >/dev/null & ;;
      home) adb -s "$serial" shell input keyevent KEYCODE_HOME & ;;
      *) ;;
    esac
  done
  wait
}

drive_all() {
  while true; do
    tap_all home
    sleep 3
    tap_all swipe 0.5 0.85 0.5 0.25 # the app drawer
    sleep 3
    tap_all swipe 0.5 0.3 0.5 0.85
    sleep 2
    tap_all settings
    sleep 3
    tap_all swipe 0.5 0.8 0.5 0.3 # scroll Settings
    sleep 2
    tap_all swipe 0.5 0.3 0.5 0.8
    sleep 2
  done
}

for i in "${!names[@]}"; do
  stream "${names[$i]}" "${serials[$i]}" &
  pids+=($!)
  echo "live-demo: ${serials[$i]} -> ${media}/${names[$i]}/whep"
done
if $drive; then
  drive_all &
  pids+=($!)
fi
wait
