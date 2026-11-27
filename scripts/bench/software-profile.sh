#!/usr/bin/env bash
# Android software mode (Linux without KVM): a smaller screen and a 30 Hz display, alone and
# together, on one AVD.
#
#   scripts/bench/software-profile.sh --qemu QEMU (or EMU_QEMU) [--image DIR] [--arms "default lcd540 vsync30 lite"]
#                                     [--memory MB] [--settle S] [--window S] [--port N]
#                                     [--rounds N] [--quick] [--out DIR]
#
# One AVD (emu -create-avd, 4 vCPUs, --memory MB, 720x1280@320) for the whole run, deleted at the
# end. Its first boot is uncounted. Each round then boots it warm under every arm in turn (the
# order alternating between rounds), rewriting the AVD's config.ini first:
#   default  720x1280 at 320 dpi, 60 Hz (hw.lcd.vsync unset)
#   lcd540   540x960 at 240 dpi, 60 Hz
#   vsync30  720x1280 at 320 dpi, hw.lcd.vsync=30
#   lite     540x960 at 240 dpi, hw.lcd.vsync=30 (the software-lite profile)
# Boots use -no-window -no-audio -no-boot-anim -no-snapshot-save. Per boot: seconds from launch
# until sys.boot_completed and until the launcher has focus; the device then sits --settle s (240)
# and idle host CPU is the emulator's process tree's CPU time over the next --window s (60), in
# cores; then two runs of scripts/android-jank.sh (10 launcher swipes up and down over adb): the
# frames the launcher drew, frames per second of the run, janky frames, frame time percentiles
# and host cores while swiping; last, the process tree's proportional set size (shared guest RAM
# counted once) and its peak resident size sampled during the boot.
#
# Needs an x86_64 Linux host with no /dev/kvm, QEMU 11 or newer (--qemu), the renderer
# (`emu -provision-renderer`) and the API 36 default x86_64 image (--image, default
# system-images/android-36/default/x86_64 under $ANDROID_HOME). --quick: one round of the default
# and lite arms with --settle 20 --window 20, a smoke test and not a number to publish.
# Reproduces docs/android.md's software-lite table.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
qemu=${EMU_QEMU:-}
image=system-images/android-36/default/x86_64
arms="default lcd540 vsync30 lite"
memory=1536
settle=""
window=""
port=5740
while [ $# -gt 0 ]; do
  case $1 in
    --qemu)
      qemu=$2
      shift 2
      ;;
    --image)
      image=$2
      shift 2
      ;;
    --arms)
      arms=$2
      shift 2
      ;;
    --memory)
      memory=$2
      shift 2
      ;;
    --settle)
      settle=$2
      shift 2
      ;;
    --window)
      window=$2
      shift 2
      ;;
    --port)
      port=$2
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
if [ "$BENCH_QUICK" = 1 ]; then
  arms="default lite"
  settle=${settle:-20}
  window=${window:-20}
fi
settle=${settle:-240}
window=${window:-60}
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then bench_skip "software mode is measured on x86_64 Linux"; fi
if [ -e /dev/kvm ]; then bench_skip "this host has /dev/kvm; software mode is for hosts without it"; fi
if [ -z "$qemu" ] || [ ! -x "$qemu" ]; then bench_skip "needs --qemu or EMU_QEMU: qemu-system-x86_64, 11 or newer"; fi
android_env
image_dir "$image"
android_port "$port"
bench_start software-profile
emu_ready

# arm_config ARM: width height density vsync (0 leaves hw.lcd.vsync out).
arm_config() {
  case $1 in
    default) echo "720 1280 320 0" ;;
    lcd540) echo "540 960 240 0" ;;
    vsync30) echo "720 1280 320 30" ;;
    lite) echo "540 960 240 30" ;;
    *) bench_die "arm $1: expected default, lcd540, vsync30 or lite" ;;
  esac
}
# set_key FILE KEY VALUE: replaces the key's line or appends one; VALUE empty removes it.
set_key() {
  sed -i "/^$2=/d" "$1"
  [ -z "$3" ] || echo "$2=$3" >>"$1"
}
apply_arm() {
  local cfg w h d v
  cfg="$BENCH_AVD_HOME/$avd.avd/config.ini"
  read -r w h d v <<<"$(arm_config "$1")"
  set_key "$cfg" hw.lcd.width "$w"
  set_key "$cfg" hw.lcd.height "$h"
  set_key "$cfg" hw.lcd.density "$d"
  set_key "$cfg" hw.lcd.vsync "$([ "$v" = 0 ] || echo "$v")"
}
# pss_mb PID...: proportional set size of the processes and their descendants, MB.
pss_mb() {
  local p
  for p in $(tree_pids "$@"); do
    [ -r "/proc/$p/smaps_rollup" ] && awk '/^Pss:/ { s += $2 } END { print s + 0 }' "/proc/$p/smaps_rollup"
  done | awk '{ s += $1 } END { printf "%d\n", s / 1024 }'
}
# swipe_run LABEL: one scripts/android-jank.sh run, sampled.
swipe_run() {
  local label=$1 t0 out secs frames cpu
  t0=$(now)
  out=$("$BENCH_REPO/scripts/android-jank.sh" "emulator-$port" "$BOOT_PID" 2>/dev/null)
  secs=$(since "$t0" 1)
  field() { printf '%s\n' "$out" | tr ' ' '\n' | sed -n "s/^$1=//p"; }
  frames=$(field frames)
  cpu=$(field host_cpu_pct)
  bench_sample "$a" "$label: frames rendered in 20 swipes" "${frames:-0}"
  bench_sample "$a" "$label: frames per second of the run" "$(awk -v f="${frames:-0}" -v s="$secs" 'BEGIN { printf "%.2f", f / s }')"
  bench_sample "$a" "$label: janky frames, %" "$(field janky_pct)"
  bench_sample "$a" "$label: frame time p50, ms" "$(field p50)"
  bench_sample "$a" "$label: frame time p90, ms" "$(field p90)"
  bench_sample "$a" "$label: host cores while swiping" "$(awk -v c="${cpu:-0}" 'BEGIN { printf "%.2f", c / 100 }')"
}

avd="bench-profile-$$"
avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory "$memory" -lcd 720x1280@320
qemu_env="emu:EMU_QEMU=$qemu"
BENCH_ROUND=0
apply_arm default
android_boot "$qemu_env" "$avd" "$port" "$BENCH_WORK/first-boot.log" || bench_die "the AVD's first boot failed; see $BENCH_WORK/first-boot.log"
bench_log "first boot ${BOOT_S}s (not counted)"
sleep 60
android_stop "$BOOT_PID" "$port"

read -r -a arm_list <<<"$arms"
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${arm_list[@]}")
  if [ $((BENCH_ROUND % 2)) = 0 ]; then
    order=()
    for ((i = ${#arm_list[@]} - 1; i >= 0; i--)); do order+=("${arm_list[i]}"); done
  fi
  for a in "${order[@]}"; do
    apply_arm "$a"
    t0=$(now)
    if ! android_boot "$qemu_env" "$avd" "$port" "$BENCH_WORK/boot-$BENCH_ROUND-$a.log"; then
      bench_sample "$a" "warm boot: launch until sys.boot_completed, s" never
      android_stop "$BOOT_PID" "$port"
      continue
    fi
    bench_sample "$a" "warm boot: launch until sys.boot_completed, s" "$BOOT_S"
    bench_sample "$a" "warm boot: launch until the launcher has focus, s" "$(launcher_s "emulator-$port" "$t0" 120)"
    sleep "$settle"
    c0=$(cpu_s "$BOOT_PID")
    w0=$(now)
    sleep "$window"
    c1=$(cpu_s "$BOOT_PID")
    w=$(since "$w0" 3)
    bench_sample "$a" "idle host cores (${settle} s after boot, over ${window} s)" \
      "$(awk -v a="$c0" -v b="$c1" -v w="$w" 'BEGIN { printf "%.2f", (b - a) / w }')"
    swipe_run "swipes, first run"
    swipe_run "swipes, second run"
    bench_sample "$a" "memory: process tree PSS after the swipes, MB" "$(pss_mb "$BOOT_PID")"
    bench_sample "$a" "memory: peak resident size sampled in the boot, MB" "$PEAK_RSS_MB"
    android_stop "$BOOT_PID" "$port"
  done
done
bench_finish "default,lcd540,vsync30,lite" "Image: $IMAGE_DIR, AVD 4 vCPUs, $memory MB; warm boots (one uncounted first boot) with -no-window -no-audio -no-boot-anim -no-snapshot-save; QEMU $("$qemu" --version | head -1)."
