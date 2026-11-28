#!/usr/bin/env bash
# Android renderers: emu's host Vulkan and host GLES drivers against each other, per API (#355).
#
#   scripts/bench/renderers.sh [--api "36 30"] [--arms "mvk-metal ..."] [--lib64 DIR] [--lcd WxH@DPI]
#                              [--settle S] [--window S] [--shots DIR] [--avd PREFIX] [--port N]
#                              [--ref ARM] [--boot-gate SCRIPT] [--no-cold-init] [--cold-only] [--dry-run]
#                              [--rounds N] [--quick] [--out DIR]
#
# Arms (host Vulkan under gfxstream + the renderer's host GLES, ANGLE):
#   mvk-metal     MoltenVK + ANGLE on Metal       -gpu host, ANDROID_EMU_VK_ICD=moltenvk,
#                                                 ANGLE_DEFAULT_PLATFORM=metal
#   mvk-swangle   MoltenVK + SwANGLE              -gpu host, ANDROID_EMU_VK_ICD=moltenvk,
#                                                 ANGLE_DEFAULT_PLATFORM=swiftshader
#   kk-metal      KosmicKrisp + ANGLE on Metal    -gpu host, ANDROID_EMU_VK_ICD=kosmickrisp and
#                                                 VK_DRIVER_FILES/VK_ICD_FILENAMES at the renderer's
#                                                 lib64/vulkan/libkosmickrisp_icd.json
#   kk-swangle    KosmicKrisp + SwANGLE
#   lavapipe      -gpu lavapipe (software Vulkan; host GLES as emu picks it)
#   swiftshader   -gpu swiftshader (software Vulkan; host GLES as emu picks it)
# Under -gpu host, ANDROID_EMU_VK_ICD=moltenvk or =kosmickrisp picks emu's driver (without it,
# API 35 runs KosmicKrisp, #412, and API 34 MoltenVK, #495), and emu's configure_environment
# (crates/emu-gfxstream/src/lib.rs) then sets ANDROID_EMU_VK_ICD, VK_DRIVER_FILES and
# VK_ICD_FILENAMES to that driver whatever they held, printing a notice when it overrides one.
# The other variables (ANGLE_DEFAULT_PLATFORM, MVK_*) are set only when unset, so the arm's win.
# `emu -warm-renderer` initializes on the driver ANDROID_EMU_VK_ICD names. gfxstream points the
# loader at the manifest ANDROID_EMU_VK_ICD names (VK_LOADER_DEBUG=driver shows which). An arm whose ICD
# the renderer doesn't ship is skipped, never downloaded.
#
# Per API (--api, default "36 30"; 33 and 34 also known, google_apis arm64 images), one AVD
# (emu -create-avd -no-golden, 4 vCPUs, 2 GB, --lcd 1080x2400@420) in a private ANDROID_AVD_HOME
# under the work directory (~/.android/avd is never touched), deleted at the end. Each round,
# every arm in turn (order alternating between rounds):
#   first boot  -wipe-data: launch until sys.boot_completed and until the launcher has focus,
#               peak RSS of the process tree, emu's `renderer initialized in` (-verbose)
#   warm boot   the same without the wipe, then --settle s (45), idle host CPU over --window s
#               (60) as % of one core, scripts/android-jank.sh (10 launcher app-drawer swipes:
#               janky frames, p90, host CPU), and two correctness screenshots under --shots:
#               the home screen (launcher icons) and Settings search with Gboard up. Each is
#               scored by its colours (a blank buffer is one flat colour) and, once the --ref
#               arm (lavapipe) has a shot of that screen on this API, by the share of pixels
#               that differ from it. Peak memory is the larger of the boot's peak RSS and the RSS after the
#               swipes.
# Every sample carries the 1-minute host load (bench_sample) and each boot records it at launch.
# A run that gives no usable numbers is a failure, never a sample: a boot that never completed,
# got no slot or crashed after sys.boot_completed. Its row shows the word (never, no-slot,
# crashed; a crashed run's other numbers are dropped), the medians leave it out, and the note
# under the table counts the failures per arm.
#
# Cold renderer init (unless --no-cold-init), device-free: `emu -warm-renderer <lib64>` (an API
# 36 boot's init, no guest) timed per arm, once cold and once warm, per round. Cold means an
# empty Metal shader cache: each run uses a copy of emu whose embedded bundle id (dev.simee.emu,
# apps/emu/build.rs) is rewritten to a new one of the same length, so Metal gives it a cache of
# its own; that cache is removed afterwards. No existing cache is touched. --cold-only runs just
# this part (no device).
#
# --boot-gate SCRIPT: hold a host boot slot for each boot (waaagh's boot-slot.sh): SCRIPT runs in
# the background with a command that waits for this boot to finish, from before the launch until
# sys.boot_completed, plus its own settle. A boot that gets no slot is recorded as no-slot.
# --dry-run prints the plan (images, arms, ICDs, AVD names, port) and exits: no build, no device.
#
# Defaults: --port 5700 (adb 5701), --avd bench-renderers-<pid> (the AVD is <prefix>-<api>),
# --shots bench/results/renderers-shots. --quick: one round, --settle 20 --window 20.
# Devices: the port must be even, and neither emulator-<port> nor emulator-<port+1> may be on
# the protected list (scripts/bench/lib.sh: emulator-5554/5556/5558/5590/5680 and
# BENCH_PROTECTED_DEVICES), whether or not that device is up. Before every boot the ports are
# checked free again; emu is told to quit on the console only while it is the process this run
# started and the one listening on the port, and is signalled only while its PID still has the
# start time and command (when it was seen) it had at launch.
# Results: one JSON per API (renderers-api<N>-*.json) and one for the inits (renderers-init-*).
# Reproduces docs/research/renderers.md.
set -uo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
apis="36 30"
arms="mvk-metal mvk-swangle kk-metal kk-swangle lavapipe swiftshader"
lib64=""
lcd=1080x2400@420
settle=""
window=""
shots=""
avd_prefix="bench-renderers-$$"
port=5700
gate=""
ref_arm=lavapipe
cold_init=1
cold_only=0
dry=0
while [ $# -gt 0 ]; do
  case $1 in
    --api)
      apis=$2
      shift 2
      ;;
    --arms)
      arms=$2
      shift 2
      ;;
    --lib64)
      lib64=$2
      shift 2
      ;;
    --lcd)
      lcd=$2
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
    --shots)
      shots=$2
      shift 2
      ;;
    --avd)
      avd_prefix=$2
      shift 2
      ;;
    --port)
      port=$2
      shift 2
      ;;
    --boot-gate)
      gate=$2
      shift 2
      ;;
    --ref)
      ref_arm=$2
      shift 2
      ;;
    --no-cold-init)
      cold_init=0
      shift
      ;;
    --cold-only)
      cold_only=1
      shift
      ;;
    --dry-run)
      dry=1
      shift
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
if [ "$BENCH_QUICK" = 1 ]; then
  settle=${settle:-20}
  window=${window:-20}
fi
settle=${settle:-45}
window=${window:-60}
shots=${shots:-$BENCH_OUT/renderers-shots}

# --- Processes this run started ----------------------------------------------------------------

# A process this script started is identified as lib.sh's bench_track identifies it: its PID, its
# start time and, where it is known, its command, recorded at launch. Nothing is signalled unless
# they all still match (hours into a run macOS can hand a finished process's PID to another).
owns_port() { # PID PORT: PID is the process listening on the console port
  case " $(lsof -nP -iTCP:"$2" -sTCP:LISTEN -t 2>/dev/null | tr '\n' ' ') " in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}
# Stops the emu a boot started (PID, its IDENT from android_boot's BOOT_IDENT, the console PORT).
# The console's quit goes out only when PID is still that process and the one on the port: the
# port alone could be another run's emulator. Then SIGINT/SIGTERM, again only while PID is still
# that process. Always untracked.
emu_stop() {
  local pid=$1 ident=$2 port=$3 i
  [ -n "$pid" ] || return 0
  if bench_pid_is "$pid" "$ident"; then
    if owns_port "$pid" "$port"; then
      "$BENCH_ADB" -s "emulator-$port" emu kill >/dev/null 2>&1
    fi
    for i in $(seq 1 $((${EMU_QUIT_WAIT:-15} * 10))); do
      bench_pid_is "$pid" "$ident" || break
      sleep 0.1
    done
    ! bench_pid_is "$pid" "$ident" || bench_stop_pid "$pid" "${EMU_STOP_GRACE:-30}" "$ident"
  elif [ -z "$ident" ]; then
    bench_log "emu $pid: no identity recorded; not signalling it"
  fi
  bench_untrack "$pid"
  return 0
}

# The boot gate: SCRIPT -- (touch go; wait for done) in the background. Each boot has its own
# pair of files, so a previous boot's waiter can't miss its done; the waiter also stops once the
# work directory is gone (the run's exit), as nothing else stops it. The waiters are kept as
# "PID|START" entries here, not in bench_track's list, whose cleanup signals at once, with no grace.
GATE_N=0
GATE_DONE=""
GATE_GRACE=${GATE_GRACE:-10}
GATE_SLOTS=()
gate_live() { bench_pid_is "${1%%|*}" "${1#*|}|"; }
gate_reap() { # forget the waiters that have finished
  local keep=() e
  for e in ${GATE_SLOTS[@]+"${GATE_SLOTS[@]}"}; do gate_live "$e" && keep+=("$e"); done
  GATE_SLOTS=(${keep[@]+"${keep[@]}"})
}
gate_acquire() {
  GATE_PID=""
  [ -n "$gate" ] || return 0
  gate_reap
  GATE_N=$((GATE_N + 1))
  local go="$BENCH_WORK/gate-go-$GATE_N" entry
  GATE_DONE="$BENCH_WORK/gate-done-$GATE_N"
  [ "$GATE_N" = 1 ] && bench_on_exit gate_cleanup
  # shellcheck disable=SC2016 # the waiter's $1 and $2 expand when it runs
  "$gate" --label "renderers $1" -- sh -c 'touch "$1"; until [ -e "$2" ] || [ ! -d "${2%/*}" ]; do sleep 1; done' \
    gate "$go" "$GATE_DONE" >>"$BENCH_WORK/gate.log" 2>&1 &
  GATE_PID=$!
  entry="$GATE_PID|$(bench_pid_start "$GATE_PID")"
  GATE_SLOTS+=("$entry")
  until [ -e "$go" ]; do
    gate_live "$entry" || {
      [ -e "$go" ] && break
      GATE_PID=""
      gate_reap
      return 1
    }
    sleep 1
  done
}
gate_release() {
  [ -n "$GATE_DONE" ] && touch "$GATE_DONE"
  GATE_PID=""
  return 0
}
# At exit: release the current waiter, give each --boot-gate process GATE_GRACE s to finish, and
# stop the ones still running that are still the processes this run started (SIGINT, then SIGTERM
# after GATE_GRACE s).
gate_cleanup() {
  local e i
  gate_release
  for e in ${GATE_SLOTS[@]+"${GATE_SLOTS[@]}"}; do
    for i in $(seq 1 $((GATE_GRACE * 10))); do
      gate_live "$e" || break
      sleep 0.1
    done
    ! gate_live "$e" || bench_stop_pid "${e%%|*}" "$GATE_GRACE" "${e#*|}|"
  done
  GATE_SLOTS=()
}

# --- Failures ----------------------------------------------------------------------------------

# A run's samples are held in a buffer until the run is known good: buf_begin, then buf_end keep
# (they join the samples) or buf_end drop.
BUF_REAL=""
buf_begin() {
  BUF_REAL=$BENCH_SAMPLES
  BENCH_SAMPLES="$BENCH_WORK/run.tsv"
  : >"$BENCH_SAMPLES"
}
buf_end() {
  local buf=$BENCH_SAMPLES
  BENCH_SAMPLES=$BUF_REAL
  if [ "$1" = keep ]; then cat "$buf" >>"$BENCH_SAMPLES"; fi
  rm -f "$buf"
  return 0
}
# run_failed LABEL first|warm KIND: the run's row shows KIND (never, no-slot, crashed, ...) in
# place of a boot time, which the medians leave out, and failure_note counts it.
run_failed() {
  bench_sample "$1" "$2 boot: launch until sys.boot_completed, s" "$3"
  printf '%s\t%s boot\t%s\n' "$1" "$2" "$3" >>"$BENCH_WORK/failures.tsv"
}
failure_note() {
  local f="$BENCH_WORK/failures.tsv"
  if [ -s "$f" ]; then
    sort "$f" | uniq -c | awk '{ n = $1; sub(/^ *[0-9]+ /, ""); split($0, p, "\t")
      printf "%s%s %s %s (%d)", (k++ ? "; " : ""), p[1], p[2], p[3], n }
      END { printf ".\n" }' | sed 's/^/Failed runs, left out of the medians: /'
  else echo "Failed runs: none."; fi
}

# --- Boots (functions) -------------------------------------------------------------------------

# The console and adb ports are free before a boot: a stopped emu may take a moment to let go,
# but whatever still holds them after that isn't this run's, and isn't touched.
port_clear() {
  local i
  for i in $(seq 1 30); do
    port_free "$port" && return 0
    sleep 1
  done
  bench_die "port $port or $((port + 1)) is in use before a boot, and not by this run's emulator; not touching what owns it"
}
# One boot of the arm: sets BOOT_PID, BOOT_IDENT (its identity, recorded at launch), BOOT_* and
# INIT_MS; returns 1 when it didn't boot and 2 without a boot slot. The slot wait can be long, so
# the ports are checked after it, right before the launch.
boot_arm() {
  local a=$1 log=$2 envs
  shift 2
  envs=$(arm_env "$a")
  BOOT_PID=""
  BOOT_IDENT=""
  if ! gate_acquire "$a"; then
    bench_log "$a: no boot slot (see $BENCH_WORK/gate.log)"
    BOOT_S=no-slot
    return 2
  fi
  port_clear
  LAUNCH_LOAD=$(load1)
  BOOT_T0=$(now)
  android_boot "emu${envs:+:$envs}" "$AVD" "$port" "$log" -gpu "$(arm_gpu "$a")" "$@"
  local rc=$?
  gate_release
  INIT_MS=$(sed -n 's/.*renderer initialized in \([0-9]*\) ms.*/\1/p' "$log" | head -1)
  return "$rc"
}
boot_alive() { bench_pid_is "${BOOT_PID:-}" "${BOOT_IDENT:-}"; }
stop_boot() {
  emu_stop "${BOOT_PID:-}" "${BOOT_IDENT:-}" "$port"
  BOOT_PID=""
  BOOT_IDENT=""
}
# --- The arms (functions; $lib64 is set by the caller) ---------------------------------------

label() {
  case $1 in
    mvk-metal) echo "MoltenVK + ANGLE-Metal" ;;
    mvk-swangle) echo "MoltenVK + SwANGLE" ;;
    kk-metal) echo "KosmicKrisp + ANGLE-Metal" ;;
    kk-swangle) echo "KosmicKrisp + SwANGLE" ;;
    lavapipe) echo "lavapipe" ;;
    swiftshader) echo "SwiftShader" ;;
    *) echo "$1" ;;
  esac
}
# The arm's environment, comma-separated K=V (android_boot's emu:K=V form). Every Vulkan arm names
# its ICD, MoltenVK's included: emu's default driver depends on the API level (KosmicKrisp where
# MoltenVK can't compile a guest's pipelines), so an unset one would mean different drivers on
# different APIs.
arm_env() {
  local kk="$lib64/vulkan/libkosmickrisp_icd.json"
  case $1 in
    mvk-metal) echo "ANDROID_EMU_VK_ICD=moltenvk,ANGLE_DEFAULT_PLATFORM=metal" ;;
    mvk-swangle) echo "ANDROID_EMU_VK_ICD=moltenvk,ANGLE_DEFAULT_PLATFORM=swiftshader" ;;
    kk-metal) echo "ANDROID_EMU_VK_ICD=kosmickrisp,VK_DRIVER_FILES=$kk,VK_ICD_FILENAMES=$kk,ANGLE_DEFAULT_PLATFORM=metal" ;;
    kk-swangle) echo "ANDROID_EMU_VK_ICD=kosmickrisp,VK_DRIVER_FILES=$kk,VK_ICD_FILENAMES=$kk,ANGLE_DEFAULT_PLATFORM=swiftshader" ;;
    *) echo "" ;;
  esac
}
arm_gpu() {
  case $1 in
    lavapipe | swiftshader) echo "$1" ;;
    *) echo host ;;
  esac
}
# The environment for the arm's renderer-only init (`emu -warm-renderer`, which configures
# MoltenVK unless told otherwise; the software arms get the ANGLE emu gives an API 36 boot).
init_env() {
  local vk="$lib64/vulkan"
  case $1 in
    lavapipe) echo "ANDROID_EMU_VK_ICD=lavapipe,VK_DRIVER_FILES=$vk/lvp_icd.json,VK_ICD_FILENAMES=$vk/lvp_icd.json,ANGLE_DEFAULT_PLATFORM=swiftshader" ;;
    swiftshader) echo "ANDROID_EMU_VK_ICD=swiftshader,VK_DRIVER_FILES=$vk/vk_swiftshader_icd.json,VK_ICD_FILENAMES=$vk/vk_swiftshader_icd.json,ANGLE_DEFAULT_PLATFORM=swiftshader" ;;
    *) arm_env "$1" ;;
  esac
}
arm_icd() {
  case $1 in
    mvk-*) echo MoltenVK_icd.json ;;
    kk-*) echo libkosmickrisp_icd.json ;;
    lavapipe) echo lvp_icd.json ;;
    swiftshader) echo vk_swiftshader_icd.json ;;
    *) echo "" ;;
  esac
}

# RENDERERS_SOURCE_ONLY=1: only the functions above (scripts/bench/test-renderers.sh).
[ "${RENDERERS_SOURCE_ONLY:-0}" = 1 ] && return 0

image_for() {
  case $1 in
    30 | 33 | 34 | 36) echo "system-images/android-$1/google_apis/arm64-v8a" ;;
    *) echo "" ;;
  esac
}
# The SDK and renderer, without failing (the dry run works on a host with neither).
if [ -z "${ANDROID_HOME:-}" ]; then
  if [ "$(uname -s)" = Darwin ]; then ANDROID_HOME=$HOME/Library/Android/sdk; else ANDROID_HOME=$HOME/Android/Sdk; fi
fi
# emu's own choice (emu_sdk::renderer::find_lib64): the SDK emulator's lib64, else simee's.
if [ -z "$lib64" ]; then
  lib64=$ANDROID_HOME/emulator/lib64
  [ -f "$lib64/libgfxstream_backend.dylib" ] || [ -f "$lib64/libgfxstream_backend.so" ] || lib64=$ANDROID_HOME/simee/renderer/current/lib64
fi
read -r -a arm_list <<<"$arms"
read -r -a api_list <<<"$apis"
for a in "${arm_list[@]}"; do [ -n "$(arm_icd "$a")" ] || bench_die "unknown arm $a (--help lists them)"; done
for v in "${api_list[@]}"; do [ -n "$(image_for "$v")" ] || bench_die "unknown API $v (30, 33, 34 or 36)"; done
# The device this run drives: refused when its serial or its adb port's is protected, up or not.
if [ "$cold_only" = 0 ]; then
  bench_refuse_protected_port "$port"
  port=$((10#$port))
fi
# Arms whose ICD this renderer ships.
usable=()
for a in "${arm_list[@]}"; do
  if [ -f "$lib64/vulkan/$(arm_icd "$a")" ]; then usable+=("$a"); else bench_log "arm $a skipped: no $(arm_icd "$a") in $lib64/vulkan (not downloaded)"; fi
done

if [ "$dry" = 1 ]; then
  echo "renderers.sh plan (dry run: nothing built, created or booted)"
  echo "  renderer lib64: $lib64"
  echo "  rounds: $BENCH_ROUNDS; settle ${settle} s, idle window ${window} s; lcd $lcd; port $port (adb $((port + 1)))"
  echo "  shots: $shots; reference arm: $ref_arm; boot gate: ${gate:-none}; cold init: $([ "$cold_init" = 1 ] && echo yes || echo no)$([ "$cold_only" = 1 ] && echo ' (only)')"
  for a in "${arm_list[@]}"; do
    icd=$(arm_icd "$a")
    have=missing
    [ -f "$lib64/vulkan/$icd" ] && have=present
    printf '  arm %-12s %-26s -gpu %-11s ICD %s (%s) env: %s\n' "$a" "$(label "$a")" "$(arm_gpu "$a")" "$icd" "$have" "$(arm_env "$a")"
  done
  for v in "${api_list[@]}"; do
    img=$ANDROID_HOME/$(image_for "$v")
    have=missing
    [ -f "$img/system.img" ] && have=installed
    echo "  API $v: $img ($have), AVD $avd_prefix-$v"
  done
  exit 0
fi

[ "${#usable[@]}" -gt 0 ] || bench_skip "no arm's Vulkan ICD is in $lib64/vulkan"
[ "$(uname -s)" = Darwin ] || bench_skip "the arms are macOS drivers (Metal, MoltenVK, KosmicKrisp)"
if [ "$cold_only" = 0 ]; then
  android_env
  android_port "$port"
fi
bench_start renderers
emu_ready
mkdir -p "$shots"
serial="emulator-$port"
# Private AVD home: this run's AVDs never land in (or collide with) ~/.android/avd; the directory
# goes with the work directory, and nothing else is removed. A kept device keeps its home too.
if [ "$cold_only" = 0 ]; then
  export ANDROID_AVD_HOME="$BENCH_WORK/avd"
  BENCH_AVD_HOME=$ANDROID_AVD_HOME
  mkdir -p "$ANDROID_AVD_HOME"
  if [ -n "${BENCH_KEEP_DEVICES:-}" ]; then
    BENCH_KEEP_WORK=1
    bench_log "BENCH_KEEP_DEVICES: the work directory (with the AVD home $ANDROID_AVD_HOME) is kept"
  fi
fi
adb_() { "$BENCH_ADB" -s "$serial" "$@"; }
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
order_for() { # round -> the arms, reversed on even rounds
  if [ $(($1 % 2)) = 0 ]; then
    local i
    for ((i = ${#usable[@]} - 1; i >= 0; i--)); do echo "${usable[i]}"; done
  else printf '%s\n' "${usable[@]}"; fi
}

# --- Cold renderer init ------------------------------------------------------------------------

cache_root=$(getconf DARWIN_USER_CACHE_DIR 2>/dev/null)
# A copy of emu under a new bundle id of the same length (dev.simee.<3 chars>), signed.
emu_copy() {
  local dir=$1 id=$2
  mkdir -p "$dir"
  ID="$id" perl -pe 'BEGIN { binmode STDIN; binmode STDOUT } s/<string>dev\.simee\.emu<\/string>/<string>$ENV{ID}<\/string>/g' \
    <"$BENCH_EMU" >"$dir/emu" || return 1
  chmod +x "$dir/emu"
  grep -q "<string>$id</string>" "$dir/emu" || return 1
  codesign -s - -f --entitlements "$BENCH_REPO/scripts/hypervisor.entitlements" "$dir/emu" >/dev/null 2>&1
}
init_once() { # arm exe -> seconds, or "failed"
  local envs=() t0 rc
  IFS=, read -r -a envs <<<"$(init_env "$1")"
  t0=$(now)
  env ${envs[@]+"${envs[@]}"} "$2" -warm-renderer "$lib64" >>"$BENCH_WORK/init.log" 2>&1
  rc=$?
  if [ "$rc" = 0 ]; then since "$t0" 3; else echo failed; fi
}
cold_inits() {
  local a id dir n=0 c w
  for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
    for a in $(order_for "$BENCH_ROUND"); do
      n=$((n + 1))
      id=$(printf 'dev.simee.%s' "$(printf '%03d' $((($$ + n) % 1000)))")
      [ -e "$cache_root/$id" ] && bench_die "Metal cache $cache_root/$id exists; not touching it"
      dir="$BENCH_WORK/init-$n"
      emu_copy "$dir" "$id" || bench_die "could not make a copy of $BENCH_EMU under bundle $id"
      bench_on_exit rm -rf "$cache_root/$id"
      c=$(init_once "$a" "$dir/emu")
      w=$(init_once "$a" "$dir/emu")
      bench_sample "$(label "$a")" "renderer init, Metal cache cold, s" "$c"
      bench_sample "$(label "$a")" "renderer init, Metal cache warm, s" "$w"
      rm -rf "${cache_root:?}/$id" "$dir"
    done
  done
}

# --- Boots -------------------------------------------------------------------------------------

# score TOP RAW [REF] -> "colors=N flat=F diff=D" over the rows from the fraction TOP down, every
# 4th pixel: colours at 5 bits a channel, the dominant colour's share (1 = one flat colour, a
# blank buffer), and the share of pixels more than 32 off REF's in any channel. RAW is
# `screencap` without -p: width, height, format (and a colour space on newer images), then RGBA.
score() {
  python3 -I -c '
import struct, sys
def load(p):
    d = open(p, "rb").read()
    w, h = struct.unpack("<II", d[:8])
    return w, h, d[len(d) - w * h * 4:]
top = float(sys.argv[1])
w, h, px = load(sys.argv[2])
ref = load(sys.argv[3]) if len(sys.argv) > 3 else None
ref = ref if ref and ref[:2] == (w, h) else None
counts, n, differ = {}, 0, 0
for y in range(int(h * top), h, 4):
    for x in range(0, w, 4):
        i = (y * w + x) * 4
        k = (px[i] >> 3, px[i + 1] >> 3, px[i + 2] >> 3)
        counts[k] = counts.get(k, 0) + 1
        n += 1
        if ref:
            differ += max(abs(px[i + j] - ref[2][i + j]) for j in range(3)) > 32
flat = max(counts.values()) / n if n else 1
diff = f"{100 * differ / n:.1f}" if ref else "none"
print(f"colors={len(counts)} flat={flat:.3f} diff={diff}")' "$@"
}
ime_shown() {
  adb_ shell dumpsys input_method 2>/dev/null | grep -c -E 'mInputShown=true|mIsInputViewShown=true|isInputViewShown=true|mImeWindowVis=[1-9]'
}
# Home screen and Gboard over Settings search, saved and scored.
# shot TAG WHAT: the screen as $shots/TAG-WHAT.png and $BENCH_WORK/TAG-WHAT.raw (for scoring).
shot() {
  adb_ exec-out screencap -p >"$shots/$1-$2.png"
  adb_ exec-out screencap >"$BENCH_WORK/$1-$2.raw"
}
# The reference arm's latest shot of the same screen on this API (ARM TAG WHAT), if it has one.
ref_raw() {
  [ "$1" != "$ref_arm" ] || return 0
  local r
  for r in "$BENCH_WORK/${2%%-r*}"-r*-"$ref_arm-$3.raw"; do [ -s "$r" ] && echo "$r"; done | tail -1
}
correctness() {
  local a=$1 tag=$2 line ref w h
  adb_ shell input keyevent KEYCODE_HOME
  sleep 3
  shot "$tag" home
  ref=$(ref_raw "$a" "$tag" home)
  # Below the status bar: the icons, the dock, the search bar.
  line=$(score 0.06 "$BENCH_WORK/$tag-home.raw" ${ref:+"$ref"})
  bench_sample "$(label "$a")" "home screen: colours (5-bit)" "$(field colors "$line")"
  bench_sample "$(label "$a")" "home screen: share of the dominant colour" "$(field flat "$line")"
  [ -n "$ref" ] && bench_sample "$(label "$a")" "home screen: pixels differing from the reference arm's, %" "$(field diff "$line")"
  adb_ shell am start -W -a com.android.settings.action.SETTINGS_SEARCH >/dev/null 2>&1
  sleep 4
  if [ "$(ime_shown)" = 0 ]; then
    read -r w h < <(adb_ shell wm size | tail -1 | tr -d '\r' | sed 's/.*: //; s/x/ /')
    adb_ shell input tap $((w / 2)) $((h * 8 / 100))
    sleep 3
  fi
  local shown
  shown=$(ime_shown)
  shot "$tag" gboard
  ref=$(ref_raw "$a" "$tag" gboard)
  # The keyboard: the bottom 30% of a phone screen.
  line=$(score 0.70 "$BENCH_WORK/$tag-gboard.raw" ${ref:+"$ref"})
  bench_sample "$(label "$a")" "Gboard shown (dumpsys input_method)" "$([ "${shown:-0}" -gt 0 ] && echo 1 || echo 0)"
  bench_sample "$(label "$a")" "Gboard region: colours (5-bit)" "$(field colors "$line")"
  bench_sample "$(label "$a")" "Gboard region: share of the dominant colour (1 = blank)" "$(field flat "$line")"
  [ -n "$ref" ] && bench_sample "$(label "$a")" "Gboard region: pixels differing from the reference arm's, %" "$(field diff "$line")"
  adb_ shell input keyevent KEYCODE_BACK
  adb_ shell input keyevent KEYCODE_HOME
}
# The warm boot's measures, after boot_arm: returns 1 when emu died (crashed) before they were
# complete and 2 when the CPU reading is unusable. The caller drops the samples of such a run.
warm_measure() {
  local a=$1 l=$2 line c0 c1 w0 w r peak idle
  bench_sample "$l" "warm boot: launch until sys.boot_completed, s" "$BOOT_S"
  bench_sample "$l" "warm boot: launch until the launcher has focus, s" "$(launcher_s "$serial" "$BOOT_T0" 180)"
  bench_sample "$l" "warm boot: renderer initialized in, ms" "${INIT_MS:-none}"
  bench_sample "$l" "warm boot: host load at launch (1 min)" "$LAUNCH_LOAD"
  peak=$PEAK_RSS_MB
  sleep "$settle"
  boot_alive || return 1
  c0=$(cpu_s "$BOOT_PID")
  w0=$(now)
  sleep "$window"
  c1=$(cpu_s "$BOOT_PID")
  w=$(since "$w0" 3)
  # A dead process reads as 0 CPU-seconds: check it is the same live emu before using c0 and c1.
  boot_alive || return 1
  idle=$(awk -v a="$c0" -v b="$c1" -v w="$w" 'BEGIN { printf "%.1f", 100 * (b - a) / w }')
  case $idle in -* | "") return 2 ;; *) ;; esac
  bench_sample "$l" "idle host CPU, % of a core (${settle} s after boot, over ${window} s)" "$idle"
  line=$(ANDROID_HOME=$BENCH_SDK "$BENCH_REPO/scripts/android-jank.sh" "$serial" "$BOOT_PID" 2>/dev/null | tail -1)
  boot_alive || return 1
  bench_log "$a: $line"
  bench_sample "$l" "swipes: janky frames, %" "$(field janky_pct "$line")"
  bench_sample "$l" "swipes: frame time p90, ms" "$(field p90 "$line")"
  bench_sample "$l" "swipes: frames rendered" "$(field frames "$line")"
  bench_sample "$l" "swipes: host CPU, % of a core" "$(field host_cpu_pct "$line")"
  r=$(rss_mb "$BOOT_PID")
  [ "${r:-0}" -gt "$peak" ] && peak=$r
  bench_sample "$l" "peak RSS (boot, or after the swipes), MB" "$peak"
  correctness "$a" "api$api-r$BENCH_ROUND-$a"
  boot_alive || return 1
}
run_api() {
  local api=$1 a l rc
  image_dir "$(image_for "$api")"
  AVD="$avd_prefix-$api"
  avd_create "$AVD" "$IMAGE_DIR" -cores 4 -memory 2048 -lcd "$lcd"
  : >"$BENCH_WORK/failures.tsv"
  # The AVD's very first boot formats /data; uncounted, so each arm's first boot is a -wipe-data.
  BENCH_ROUND=0
  boot_arm "${usable[0]}" "$BENCH_WORK/api$api-setup.log" && bench_log "API $api setup boot ${BOOT_S}s (not counted)"
  stop_boot
  for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
    for a in $(order_for "$BENCH_ROUND"); do
      l=$(label "$a")
      # First boot.
      buf_begin
      if boot_arm "$a" "$BENCH_WORK/api$api-r$BENCH_ROUND-$a-first.log" -wipe-data; then
        bench_sample "$l" "first boot: launch until sys.boot_completed, s" "$BOOT_S"
        bench_sample "$l" "first boot: launch until the launcher has focus, s" "$(launcher_s "$serial" "$BOOT_T0" 180)"
        bench_sample "$l" "first boot: renderer initialized in, ms" "${INIT_MS:-none}"
        bench_sample "$l" "first boot: peak RSS of the process tree, MB" "$PEAK_RSS_MB"
        bench_sample "$l" "first boot: host load at launch (1 min)" "$LAUNCH_LOAD"
        if boot_alive; then buf_end keep; else
          buf_end drop
          run_failed "$l" first crashed
        fi
      else
        buf_end drop
        run_failed "$l" first "${BOOT_S:-never}"
      fi
      stop_boot
      # Warm boot.
      buf_begin
      if ! boot_arm "$a" "$BENCH_WORK/api$api-r$BENCH_ROUND-$a-warm.log"; then
        buf_end drop
        run_failed "$l" warm "${BOOT_S:-never}"
        stop_boot
        continue
      fi
      warm_measure "$a" "$l"
      rc=$?
      case $rc in
        0) buf_end keep ;;
        *)
          buf_end drop
          rm -f "$BENCH_WORK/api$api-r$BENCH_ROUND-$a"-*.raw
          run_failed "$l" warm "$([ "$rc" = 1 ] && echo crashed || echo cpu-read)"
          ;;
      esac
      stop_boot
    done
  done
  failure_note >"$BENCH_WORK/failure-note.txt"
  bench_log "API $api: $(cat "$BENCH_WORK/failure-note.txt")"
  avd_delete "$AVD"
}

if [ "$cold_init" = 1 ]; then
  BENCH_NAME=renderers-init
  cold_inits
  bench_finish "$(for a in "${usable[@]}"; do label "$a"; done | paste -sd, -)" \
    "emu -warm-renderer $lib64 (an API 36 boot's renderer init, no guest), timed from launch to exit; cold: a copy of emu under a new bundle id, so Metal's shader cache for it is empty; warm: the same copy again."
  : >"$BENCH_SAMPLES"
fi
[ "$cold_only" = 1 ] && exit 0
for api in "${api_list[@]}"; do
  if [ ! -f "$BENCH_SDK/$(image_for "$api")/system.img" ]; then
    bench_log "API $api skipped: no image at $BENCH_SDK/$(image_for "$api") (not downloaded)"
    continue
  fi
  BENCH_NAME="renderers-api$api"
  run_api "$api"
  bench_finish "$(for a in "${usable[@]}"; do label "$a"; done | paste -sd, -)" \
    "API $api google_apis arm64 (AVD created ${AVD_MODE:-fresh}), 4 vCPUs, 2 GB, $lcd; -no-window -no-audio -no-boot-anim -no-snapshot-save; renderer $lib64; shots in $shots; $BENCH_ROUNDS first boots and $BENCH_ROUNDS warm boots per arm. $(cat "$BENCH_WORK/failure-note.txt")"
  : >"$BENCH_SAMPLES"
done
