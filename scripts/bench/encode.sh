#!/usr/bin/env bash
# H.264 encode cost of a live device: simee-encode's VideoToolbox encoder fed straight from the
# device (an iOS framebuffer IOSurface, or emu's frame ring copied once into an IOSurface pool).
#
#   scripts/bench/encode.sh [--platform ios|android] [--udid UDID | --avd NAME]
#                           [--device-type TYPE] [--runtime ID] [--image DIR] [--lcd WxH@DPI]
#                           [--fps "30 60"] [--seconds S] [--max-size N] [--profile cb|main]
#                           [--bitrate BPS] [--port N] [--rounds N] [--quick] [--out DIR]
#
# With --udid (a booted simulator) or --avd (a running AVD) it measures that device and leaves it
# alone. Otherwise it creates its own (iOS: simctl create --device-type in --runtime; Android:
# emu -create-avd from --image with --lcd, booted with this checkout's emu on --port), boots it,
# waits 30 s, and deletes it on exit.
#
# Each round, for each rate in --fps, runs the encode_bench example for --seconds (10) with the
# screen kept moving (home screen pages on iOS, the app drawer on Android, a swipe every 0.7 s)
# and records: frames encoded per second, per-frame encode latency median and p95 (picture
# handed to VideoToolbox until its access unit is out), the per-frame picture step (iOS: wrapping
# the surface; Android: the one copy into the pool), CPU of the encoding process and of the
# host's VTEncoderXPCService (% of a core), output kbit/s, the first and the median IDR's size,
# and the frames the device presented (what bounds the encoded rate).
#
# Defaults: --platform ios, --device-type "iPhone 17 Pro", the newest iOS runtime; Android
# --image system-images/android-36/google_apis/arm64-v8a, --lcd 1080x2400@420, --port 5730;
# --fps "30 60", --seconds 10, the encoder's defaults (Constrained Baseline, 4 Mbit/s average,
# 8 Mbit/s ceiling, full size). --quick: one round of 5 s.
# Reproduces docs/streaming.md "Encode cost".
set -uo pipefail
. "$(dirname "$0")/lib.sh"
platform=ios
udid=""
avd=""
type="iPhone 17 Pro"
runtime=""
image=system-images/android-36/google_apis/arm64-v8a
lcd=1080x2400@420
rates="30 60"
seconds=10
port=5730
extra=()
while [ $# -gt 0 ]; do
  case $1 in
    --platform)
      platform=$2
      shift 2
      ;;
    --udid)
      udid=$2
      platform=ios
      shift 2
      ;;
    --avd)
      avd=$2
      platform=android
      shift 2
      ;;
    --device-type)
      type=$2
      shift 2
      ;;
    --runtime)
      runtime=$2
      shift 2
      ;;
    --image)
      image=$2
      shift 2
      ;;
    --lcd)
      lcd=$2
      shift 2
      ;;
    --fps)
      rates=$2
      shift 2
      ;;
    --seconds)
      seconds=$2
      shift 2
      ;;
    --port)
      port=$2
      shift 2
      ;;
    --max-size | --profile | --bitrate)
      extra+=("$1" "$2")
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && seconds=5
[ "$(uname -s)" = Darwin ] || bench_skip "VideoToolbox needs macOS"
case $platform in
  ios) ios_env ;;
  android)
    android_env
    [ -n "$avd" ] || {
      image_dir "$image"
      android_port "$port"
    }
    ;;
  *) bench_die "--platform $platform: expected ios or android" ;;
esac
# A device named with --udid or --avd is the user's, booted for this run: not foreign to the gate.
bench_target ${udid:+"$udid"} ${avd:+"$avd"}
bench_start encode
bench_log "building the encode_bench example"
(cd "$BENCH_REPO" && cargo build -q --release -p simee-encode --example encode_bench) || bench_die "cargo build --example encode_bench failed"
bin="$BENCH_REPO/target/release/examples/encode_bench"

if [ "$platform" = ios ]; then
  if [ -z "$udid" ]; then
    runtime=${runtime:-$(ios_runtime)}
    [ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
    ios_create "simee-bench-encode-$$" "$type" "$runtime"
    udid=$IOS_UDID
    xcrun simctl bootstatus "$udid" -b >/dev/null || bench_die "simctl boot $udid failed"
    sleep 30
  fi
  target=(--udid "$udid")
  what="iOS simulator $udid"
else
  if [ -z "$avd" ]; then
    emu_ready
    avd="bench-encode-$$"
    avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048 -lcd "$lcd"
    android_boot emu "$avd" "$port" "$BENCH_WORK/boot.log" || bench_die "emu didn't boot $avd"
    bench_on_exit android_stop "$BOOT_PID" "$port"
    sleep 30
  fi
  target=(--avd "$avd")
  what="AVD $avd"
fi

for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  for fps in $rates; do
    json="$BENCH_WORK/round-$BENCH_ROUND-$fps.json"
    if ! "$bin" "${target[@]}" --fps "$fps" --seconds "$seconds" ${extra[@]+"${extra[@]}"} >"$json" 2>"$BENCH_WORK/round-$BENCH_ROUND-$fps.log"; then
      tail -5 "$BENCH_WORK/round-$BENCH_ROUND-$fps.log" >&2
      bench_sample "$fps fps" "encoded, fps" failed
      continue
    fi
    bench_log "$(cat "$json")"
    python3 -I -c '
import json, sys
j = json.load(open(sys.argv[1]))
arm = sys.argv[2]
rows = [
    ("encoded, fps", j["fps"]),
    ("encode latency p50, ms", j["encode_ms_p50"]),
    ("encode latency p95, ms", j["encode_ms_p95"]),
    ("picture (wrap or copy) p50, ms", j["picture_ms_p50"]),
    ("encoding process CPU, % of a core", j["cpu_pct"]),
    ("VTEncoderXPCService CPU, % of a core", j["xpc_cpu_pct"]),
    ("output, kbit/s", j["kbps"]),
    ("first IDR, KB", round(j["first_keyframe_bytes"] / 1024, 1)),
    ("IDR median, KB", round(j["keyframe_bytes_p50"] / 1024, 1)),
    ("frames the device presented, fps", round(j["frames"] / j["seconds"], 1)),
]
for metric, value in rows:
    print(f"{arm}\t{metric}\t{value}")
' "$json" "$fps fps" | while IFS=$'\t' read -r arm metric value; do bench_sample "$arm" "$metric" "$value"; done
  done
done
size=$(python3 -I -c 'import json, sys; j = json.load(open(sys.argv[1])); print("%sx%s, hardware encoder %s, %s" % (j["width"], j["height"], j["hardware"], j["profile"]))' "$json" 2>/dev/null || echo "?")
arms=$(for f in $rates; do printf '%s fps,' "$f"; done)
bench_finish "${arms%,}" "$what, encoded at $size, ${seconds} s per run with a swipe every 0.7 s; ${extra[*]-encoder defaults}."
