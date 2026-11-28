#!/usr/bin/env bash
# Tap to first changed frame through `simee serve`, by stage: Android (emu) and iOS.
#
#   scripts/bench/input-latency.sh [--platform android|ios|both] [--taps N] [--arms "emu emu:K=V"]
#                                  [--image DIR] [--port N] [--name NAME] [--device-type TYPE]
#                                  [--runtime ID] [--csv-dir DIR] [--simee BIN]
#                                  [--rounds N] [--quick] [--out DIR]
#
# Each round runs crates/sim-api/examples/input_latency.rs against the device: --taps taps (60,
# after 3 warm-up), each a `touch` down on one kept API connection at a known instant, the first
# frame whose pixels change in a 64x64 region around the touch as a frame client sees it, then
# `touch` up. Per round: the median, p90 and p99 of tap to first changed frame and of each stage
# it can tell apart, and a CSV of every tap in --csv-dir (default --out).
#
# Android: an AVD on --image (1080x2400@420, 4 vCPUs, 3 GB) booted once per arm (emu, or
# emu:K=V[,K=V], an A/B arm with environment settings; EMU_FRAME_STATS=1 is always set, and the
# display stream's capture time goes in the note), 60 s to settle, the pointer location overlay
# on (`settings put system pointer_location 1`: crosshairs through the touch, drawn by a window as
# soon as the touch is dispatched), taps at (700, 1500) on the launcher's empty workspace. Frames
# come from emu's display socket through its shared ring; the guest's side from `getevent -lt` on
# the touch screen and `dumpsys SurfaceFlinger --latency` of the overlay's layer, on the guest's
# clock.
# iOS: a new --device-type (iPhone 17) simulator on --runtime (the newest), 30 s to settle,
# Spotlight opened with a swipe down (its keyboard up), taps on the `g` key at (600, 2015), whose
# preview balloon shows on touch down. Frames: the framebuffer's damage callback. The taps type
# into the search field; that is all they change.
#
# Defaults: --platform both, --arms emu, --image system-images/android-36/google_apis/arm64-v8a,
# --port 5720, --name bench-input-$$. --quick: one round, 10 taps, shorter settles.
# Reproduces docs/benchmarks.md "Input latency"; method and numbers: docs/research/input-latency.md.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
platform=both
taps=60
arms=emu
image=system-images/android-36/google_apis/arm64-v8a
port=5720
name="bench-input-$$"
type="iPhone 17"
runtime=""
csv_dir=""
while [ $# -gt 0 ]; do
  case $1 in
    --platform)
      platform=$2
      shift 2
      ;;
    --taps)
      taps=$2
      shift 2
      ;;
    --arms)
      arms=$2
      shift 2
      ;;
    --image)
      image=$2
      shift 2
      ;;
    --port)
      port=$2
      shift 2
      ;;
    --name)
      name=$2
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
    --csv-dir)
      csv_dir=$2
      shift 2
      ;;
    --simee)
      BENCH_SIMEE=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
case $platform in android | ios | both) ;; *) bench_die "--platform is android, ios or both" ;; esac
want() { [ "$platform" = both ] || [ "$platform" = "$1" ]; }
settle_android=60
settle_ios=30
if [ "$BENCH_QUICK" = 1 ]; then
  taps=10
  settle_android=20
  settle_ios=10
fi
if want android; then
  android_env
  image_dir "$image"
  android_port "$port"
  export ANDROID_HOME
fi
if want ios; then
  ios_env
  runtime=${runtime:-$(ios_runtime)}
  [ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
fi
bench_start input-latency
csv_dir=${csv_dir:-$BENCH_OUT}
mkdir -p "$csv_dir"
stamp=$(date +%Y%m%d-%H%M%S)
simee_ready
bench_log "building the input_latency example"
(cd "$BENCH_REPO" && cargo build -q --release -p simee-api --example input_latency) || bench_die "cargo build --example input_latency failed"
probe="$BENCH_REPO/target/release/examples/input_latency"
sock_dir
command -v spectra >/dev/null && spectra load >"$csv_dir/input-latency-$stamp-load-start.txt" 2>&1

# api SOCKET METHOD JSON-PARAMS: one call through `simee serve`; fails on an error reply.
api() {
  python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.sendall((json.dumps({"id": 1, "method": sys.argv[2], "params": json.loads(sys.argv[3])}) + "\n").encode())
r = json.loads(s.makefile("rb").readline())
sys.exit(0 if r.get("ok") else f"{sys.argv[2]}: {r}")' "$@"
}

# run ARM CSV PROBE-ARGS...: one round of the probe; its stages as samples.
run() {
  local arm=$1 csv=$2
  shift 2
  "$probe" --csv "$csv" "$@" >"$BENCH_WORK/probe.json" 2>"$BENCH_WORK/probe.log" || {
    tail -5 "$BENCH_WORK/probe.log" >&2
    bench_sample "$arm" "tap to changed frame, median ms" failed
    return 1
  }
  python3 -I -c '
import json, sys
s = json.load(open(sys.argv[1]))
names = {"tap -> first changed frame seen": "tap to changed frame", "touch down reply (API receive + input sent)": "touch reply",
         "guest kernel event -> SurfaceFlinger present": "guest: kernel event to present", "host: tap -> kernel + present -> seen": "host: to the kernel + present to seen"}
for st in s["stages"]:
    name = names.get(st["stage"])
    for q in ("median", "p90", "p99") if name else ():
        v = st[q + "_ms"]
        print("%s, %s ms\t%s" % (name, q, "n/a" if v is None else "%.2f" % v))
print("missed taps\t%d" % s["missed"])' "$BENCH_WORK/probe.json" | while IFS=$'\t' read -r metric value; do
    bench_sample "$arm" "$metric" "$value"
  done
}

notes=()
if want android; then
  emu_ready
  adb_s() { "$BENCH_ADB" -s "emulator-$port" "$@"; }
  avd_create "$name" "$IMAGE_DIR" -cores 4 -memory 3072 -lcd 1080x2400@420
  read -r -a arm_list <<<"$arms"
  for arm in "${arm_list[@]}"; do
    case $arm in *:*) boot_arm="$arm,EMU_FRAME_STATS=1" ;; *) boot_arm="$arm:EMU_FRAME_STATS=1" ;; esac
    log="$BENCH_WORK/boot-${arm//[^A-Za-z0-9]/_}.log"
    BENCH_ROUND=0
    if ! android_boot "$boot_arm" "$name" "$port" "$log"; then
      bench_sample "$arm" "tap to changed frame, median ms" no-boot
      android_stop "$BOOT_PID" "$port"
      continue
    fi
    sleep "$settle_android"
    adb_s shell 'settings put system pointer_location 1; settings put system screen_off_timeout 1800000; svc power stayon true; input keyevent KEYCODE_HOME' >/dev/null
    sleep 3
    sock="$SOCK_DIR/a.sock"
    rm -f "$sock"
    serve_start "$BENCH_SIMEE" "$sock"
    for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
      run "$arm" "$csv_dir/input-latency-$stamp-android-${arm//[^A-Za-z0-9]/_}-r$BENCH_ROUND.csv" \
        --socket "$sock" --device "$name" --avd-dir "$BENCH_AVD_HOME/$name.avd" --x 700 --y 1500 --taps "$taps" \
        --adb "$BENCH_ADB" --serial "emulator-$port"
    done
    bench_stop_pid "$SERVE_PID" 5
    notes+=("$arm $(grep 'emu: display:' "$log" | tail -1 | sed 's/^emu: display: //')")
    android_stop "$BOOT_PID" "$port"
  done
fi

if want ios; then
  ios_create "$name" "$type" "$runtime"
  xcrun simctl bootstatus "$IOS_UDID" -b >/dev/null || bench_die "simctl boot $IOS_UDID failed"
  sleep "$settle_ios"
  sock="$SOCK_DIR/i.sock"
  rm -f "$sock"
  serve_start "$BENCH_SIMEE" "$sock"
  # Spotlight from the home screen, then past the keyboard's first-use slide-to-type card (where
  # it isn't shown, the tap lands below the keyboard's keys).
  api "$sock" swipe "{\"device\":\"$IOS_UDID\",\"x1\":603,\"y1\":1100,\"x2\":603,\"y2\":1700,\"duration_ms\":300}" || bench_die "the swipe to Spotlight failed"
  sleep 2
  api "$sock" tap "{\"device\":\"$IOS_UDID\",\"x\":668,\"y\":2474}"
  sleep 2
  for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
    run ios "$csv_dir/input-latency-$stamp-ios-r$BENCH_ROUND.csv" --socket "$sock" --device "$IOS_UDID" --x 600 --y 2015 --taps "$taps"
  done
  bench_stop_pid "$SERVE_PID" 5
  xcrun simctl shutdown "$IOS_UDID" >/dev/null 2>&1
fi

command -v spectra >/dev/null && spectra load >"$csv_dir/input-latency-$stamp-load-end.txt" 2>&1
note="$taps taps per round after 3 warm-up; CSVs in $csv_dir (input-latency-$stamp-*)."
want android && note="$note Android: $IMAGE_DIR, 1080x2400@420, pointer location overlay; ${notes[*]:-}."
want ios && note="$note iOS: $type, $runtime, Spotlight keyboard key."
bench_finish "" "$note"
