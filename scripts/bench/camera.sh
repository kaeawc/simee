#!/usr/bin/env bash
# Camera: source to guest latency (p50, p95), CPU per linked device, and the cost of fanning one
# camera out to N devices.
#
#   scripts/bench/camera.sh [--readers 1,2,4,8] [--secs N] [--size WxH@FPS] [--camera ID]
#                           [--android SERIAL]... [--ios UDID]... [--shots N] [--no-host]
#                           [--rounds N] [--quick] [--out DIR]
#
# Each round runs, with crates/sim-camera/examples/camera-bench.rs (release build of this checkout):
#   fan-out     host-only, no device: one source fanned out to N camera feeds, one per linked
#               device as the simee server serves them, each read at the device's size, for each
#               N in --readers (1,2,4,8), --secs (5) per N. Arms: a camera-like source (1280x720
#               frames fitted to each reader's size), the same at mixed sizes, the test card, and,
#               with --camera ID (a webcam or virtual camera, as `webcam:` takes it; needs the camera
#               permission for the terminal), that camera. The readers run in their own process,
#               as a device's camera does. Per N: the server side's CPU (capture, fan-out, socket
#               writes), in all and per device; the readers' process's CPU; the slowest reader's
#               frame rate; and source to reader latency through the feed.
#   --android SERIAL  an emu device that is already booted, with its back camera linked at boot
#               (the guest reads the camera list once): its back camera is relinked to a feed this
#               run serves (flat colours numbering each frame, a centre box and its surround, which
#               survive the guest's scaling, rotation, mirroring and YUV conversion, and number 3782
#               frames before one repeats), the default camera app is opened, and --shots (30)
#               screenshots (`adb exec-out screencap`) are matched to the frames sent: latency from
#               sending a frame to a screenshot showing it. CPU per linked device: the emu process
#               while the camera app streams minus the same with the app closed, over --secs. The
#               camera's link is restored and the app closed on exit.
#   --ios UDID  a booted simulator: shims/ios-camera's fixture app (dev.simee.camerafixture, built
#               and installed here, uninstalled on exit unless it was there before) is launched with
#               the capture shim on a feed this run serves; the same shots (`simctl io screenshot`);
#               CPU per linked device: the fixture app's process while it streams. A simulator
#               already running the fixture app is skipped (its process isn't this run's to
#               restart); the run stops only the app process it launched.
# Devices are only ever the ones named: none is created, booted, shut down or picked by default,
# and a protected one is refused: the built-in list (lib.sh's BENCH_PROTECTED_BUILTIN: the owner's
# and CI's devices) plus any serial or UDID in $BENCH_PROTECTED_DEVICES (space-separated).
# Defaults: host fan-out only, 3 rounds. --quick: one round, --secs 2, readers 1,4, 10 shots.
# Not in docs/benchmarks.md yet ("Camera" would be its section).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
readers=1,2,4,8
secs=5
size=640x480@30
camera=""
shots=30
host=1
androids=()
ioses=()
while [ $# -gt 0 ]; do
  case $1 in
    --readers | --secs | --size | --camera | --shots | --android | --ios)
      [ $# -ge 2 ] || bench_die "$1 needs a value"
      case $1 in
        --readers) readers=$2 ;;
        --secs) secs=$2 ;;
        --size) size=$2 ;;
        --camera) camera=$2 ;;
        --shots) shots=$2 ;;
        --android) androids+=("$2") ;;
        --ios) ioses+=("$2") ;;
        *) ;;
      esac
      shift 2
      ;;
    --no-host)
      host=0
      shift
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
if [ "$BENCH_QUICK" = 1 ]; then
  secs=2
  readers=1,4
  shots=10
fi
bench_refuse_protected ${androids[@]+"${androids[@]}"} ${ioses[@]+"${ioses[@]}"}
[ "$host" = 1 ] || [ ${#androids[@]} -gt 0 ] || [ ${#ioses[@]} -gt 0 ] || bench_die "--no-host with no --android or --ios: nothing to run"

bench_target ${androids[@]+"${androids[@]}"} ${ioses[@]+"${ioses[@]}"}
bench_start camera
cd "$BENCH_REPO" || exit 2
bench_log "building the camera-bench example"
cargo build -q --release -p simee-camera --example camera-bench || bench_die "cargo build --release -p simee-camera --example camera-bench failed"
BIN="$BENCH_REPO/target/release/examples/camera-bench"

# Records the RESULT lines of a camera-bench log as samples of this round.
record() {
  local tag arm metric value
  while IFS=$'\t' read -r tag arm metric value; do
    [ "$tag" = RESULT ] && bench_sample "$arm" "$metric" "$value"
  done <"$1"
}

fanout() { # log, camera-bench fanout options
  local log=$1
  shift
  "$BIN" fanout --readers "$readers" --secs "$secs" --size "$size" "$@" >"$log" 2>"$log.err" || {
    tail -5 "$log.err" >&2
    bench_log "fan-out $*: failed"
    return 1
  }
  record "$log"
}

# latency_run ARM SOCKET SHOT_CMD POINT PIDS_CMD: serves the counter feed on SOCKET (the caller
# links the device's camera to it, in the background), waits for the device to connect, measures
# the CPU of the processes PIDS_CMD prints (evaluated once the device reads the feed) over --secs,
# then takes the shots.
latency_run() {
  local arm=$1 socket=$2 cmd=$3 point=$4 pids_cmd=$5 log go pid c0 c1 t0 pids
  log="$BENCH_WORK/latency-$BENCH_ROUND-$RANDOM.log"
  go="$BENCH_WORK/go-$RANDOM"
  "$BIN" latency --socket "$socket" --shot-cmd "$cmd" --shots "$shots" --point "$point" --go-file "$go" >"$log" 2>"$log.err" &
  pid=$!
  bench_track "$pid"
  for _ in $(seq 1 1200); do
    grep -q '^connected' "$log" 2>/dev/null && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  if ! grep -q '^connected' "$log" 2>/dev/null; then
    tail -5 "$log.err" >&2
    bench_stop_pid "$pid" 5
    bench_log "$arm: the device never read the feed"
    return 1
  fi
  pids=$(eval "$pids_cmd")
  if [ -n "$pids" ]; then
    # shellcheck disable=SC2086 # one PID per word
    c0=$(cpu_s $pids)
    t0=$(now)
    sleep "$secs"
    # shellcheck disable=SC2086
    c1=$(cpu_s $pids)
    bench_sample "$arm" "device CPU while streaming, % of one core" "$(cpu_pct "$c0" "$c1" "$t0")"
  else
    bench_log "$arm: no device process found to measure"
  fi
  touch "$go"
  wait "$pid"
  bench_untrack "$pid"
  awk -F'\t' -v arm="$arm" 'BEGIN { OFS = "\t" } $1 == "RESULT" { $2 = arm ": " $2 } { print }' "$log" >"$log.arm"
  record "$log.arm"
}

cpu_pct() { # cpu-seconds before, after, start time
  awk -v a="$1" -v b="$2" -v t="$(since "$3" 3)" 'BEGIN { printf "%.1f", (b - a) / t * 100 }'
}

android_round() {
  local serial=$1 port linked sock emu c0 c1 t0
  port=${serial#emulator-}
  [ "$("$BENCH_ADB" -s "$serial" get-state 2>/dev/null)" = device ] || {
    bench_log "$serial: not an attached, booted device; skipped"
    return 1
  }
  # The whole source after `back: `: a source id may hold spaces (`webcam:FaceTime HD Camera`).
  linked=$("$BENCH_ADB" -s "$serial" emu camera list 2>/dev/null | tr -d '\r' | sed -n 's/^back: //p' | head -1)
  case $linked in
    "")
      bench_log "$serial: no 'camera list' on its console (not an emu device?); skipped"
      return 1
      ;;
    none)
      bench_log "$serial: its back camera wasn't linked at boot, so the guest has none; boot it with -camera-back emulated"
      return 1
      ;;
    *) ;;
  esac
  emu=$(lsof -nP -t -iTCP:"$port" -sTCP:LISTEN 2>/dev/null | head -1)
  sock="/tmp/simee-cambench-$$-$port.sock"
  # Quoted for the exit trap's eval, so the source comes back as one word, spaces and all.
  local adb
  adb=$(printf %q "$BENCH_ADB")
  bench_on_exit "$adb -s $(printf %q "$serial") emu camera link back $(printf %q "$linked") >/dev/null 2>&1; $adb -s $(printf %q "$serial") shell input keyevent KEYCODE_HOME >/dev/null 2>&1"
  (
    for _ in $(seq 1 100); do
      [ -S "$sock" ] && break
      sleep 0.1
    done
    "$BENCH_ADB" -s "$serial" emu camera link back "feed:$sock" >/dev/null
    "$BENCH_ADB" -s "$serial" shell am start -W -a android.media.action.STILL_IMAGE_CAMERA >/dev/null
  ) &
  latency_run "android $serial" "$sock" "\"$BENCH_ADB\" -s $serial exec-out screencap -p > {}" 50,45 "echo $emu"
  wait
  if [ -n "$emu" ]; then
    "$BENCH_ADB" -s "$serial" shell input keyevent KEYCODE_HOME >/dev/null
    sleep 3
    c0=$(cpu_s "$emu")
    t0=$(now)
    sleep "$secs"
    c1=$(cpu_s "$emu")
    bench_sample "android $serial" "device CPU, camera app closed, % of one core" "$(cpu_pct "$c0" "$c1" "$t0")"
  fi
  "$BENCH_ADB" -s "$serial" emu camera link back "$linked" >/dev/null
}

# Stops the fixture app process a launch log names (`simctl launch` prints `<bundle id>: <pid>`):
# the one this run launched, never another.
ios_stop_launched() {
  local pid
  pid=$(awk -F': ' '/: [0-9]+$/ { print $2 }' "$1" 2>/dev/null | head -1)
  # Forget the PID once it's stopped, so the exit handler never signals a reused PID.
  [ -n "$pid" ] && {
    bench_stop_pid "$pid" 5
    : >"$1"
  }
  return 0
}

IOS_SHIM=""
IOS_APP=dev.simee.camerafixture
# Simulators that were already running the fixture app when this run started: skipped, since that
# app process isn't this run's to restart or stop.
IOS_BUSY=" "
ios_round() {
  local udid=$1 sock app had launched
  xcrun simctl list devices | grep -F "($udid)" | grep -q '(Booted)' || {
    bench_log "$udid: not a booted simulator; skipped"
    return 1
  }
  app=$IOS_APP
  if [ -z "$IOS_SHIM" ]; then
    IOS_SHIM="$BENCH_WORK/ios-camera"
    "$BENCH_REPO/shims/ios-camera/build.sh" "$IOS_SHIM" >"$BENCH_WORK/shim-build.log" 2>&1 || bench_die "shims/ios-camera/build.sh failed (see $BENCH_WORK/shim-build.log)"
  fi
  had=0
  xcrun simctl get_app_container "$udid" "$app" >/dev/null 2>&1 && had=1
  [ "$had" = 1 ] || {
    xcrun simctl install "$udid" "$IOS_SHIM/CameraFixture.app" || bench_die "simctl install CameraFixture.app on $udid failed"
    bench_on_exit "xcrun simctl uninstall $udid $app >/dev/null 2>&1"
  }
  sock="/tmp/simee-cambench-$$-${udid:0:8}.sock"
  launched="$BENCH_WORK/launch-$udid-$BENCH_ROUND.log"
  bench_on_exit "ios_stop_launched $(printf %q "$launched")"
  (
    for _ in $(seq 1 100); do
      [ -S "$sock" ] && break
      sleep 0.1
    done
    SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$IOS_SHIM/libsimee-camera.dylib" SIMCTL_CHILD_SIMEE_CAMERA_SOCKET="$sock" \
      xcrun simctl launch "$udid" "$app" >"$launched" 2>&1
  ) &
  # `simctl launch` prints `<bundle id>: <pid>`; by the time the app reads the feed it has.
  latency_run "ios $udid" "$sock" "xcrun simctl io $udid screenshot --type=png {} >/dev/null 2>&1" 50,60 \
    "awk -F': ' '/: [0-9]+\$/ { print \$2 }' '$launched'"
  wait
  ios_stop_launched "$launched"
}

[ ${#androids[@]} -gt 0 ] && android_env
[ ${#ioses[@]} -gt 0 ] && ios_env
for u in ${ioses[@]+"${ioses[@]}"}; do
  if xcrun simctl spawn "$u" launchctl list 2>/dev/null | grep -qF "UIKitApplication:${IOS_APP}["; then
    bench_log "$u: $IOS_APP is already running, and this run stops only what it starts; skipped"
    IOS_BUSY="$IOS_BUSY$u "
  fi
done
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  if [ "$host" = 1 ]; then
    fanout "$BENCH_WORK/fan-webcam-$BENCH_ROUND.log" --source fake-webcam
    fanout "$BENCH_WORK/fan-mixed-$BENCH_ROUND.log" --source fake-webcam --mixed
    fanout "$BENCH_WORK/fan-card-$BENCH_ROUND.log" --source pattern:testcard
    [ -n "$camera" ] && fanout "$BENCH_WORK/fan-camera-$BENCH_ROUND.log" --source "webcam:$camera"
  fi
  for s in ${androids[@]+"${androids[@]}"}; do android_round "$s"; done
  for u in ${ioses[@]+"${ioses[@]}"}; do
    case $IOS_BUSY in *" $u "*) ;; *) ios_round "$u" ;; esac
  done
done
bench_finish "" "fan-out CPU: the server side (capture, fan-out, socket writes) and the readers, in a process of their own as a device's camera is, measured apart; latency to a guest screen is an interval (the screenshot is taken between the command's start and its return): p50/p95 of its midpoint and of its upper end"
