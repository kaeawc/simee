# shellcheck shell=bash disable=SC2034
# (SC2034: the variables set here are read by the scripts that source it.)
# What every scripts/bench/*.sh shares; sourced, never run. Works on macOS's bash 3.2 (no
# associative arrays) and on Linux.
#
# Conventions (docs/benchmarks.md, "Reproducing a number"):
# - `--help` prints the script's header comment; `--rounds N` (default 3) rounds, medians;
#   `--quick` one short round (a smoke test, not a number to publish); `--out DIR` for results
#   (default bench/results, which git ignores).
# - The host is recorded at the start and the end (model, OS, CPUs, load average, simee's commit):
#   scripts/bench/summarize.py host.
# - Every device a run uses it creates itself and deletes on exit (AVDs with `emu -delete-avd`,
#   simulators with `simctl delete`, by UDID), or, where a script says so (camera.sh), it is a
#   booted device named on the command line, which the run leaves as it found it. It never touches
#   another device, nor ever one on the protected list (bench_refuse_protected), never quits
#   Simulator.app, never runs `adb kill-server`, and stops only the PIDs it started (SIGINT first).
# - Samples go through bench_sample; bench_finish writes the JSON and prints a Markdown table.

BENCH_REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BENCH_DIR="$BENCH_REPO/scripts/bench"
BENCH_ROUNDS=3
BENCH_QUICK=0
BENCH_OUT=${BENCH_OUT:-$BENCH_REPO/bench/results}
BENCH_ROUND=0
BENCH_ARGS="$*"
BENCH_PIDS=()
BENCH_CLEANUP=()
BENCH_SHIFT=1

# Devices no benchmark touches, whatever it is told: the owner's and CI's own (emulators by serial,
# simulators by UDID). $BENCH_PROTECTED_DEVICES (space-separated) adds to them, never replaces them.
BENCH_PROTECTED_BUILTIN="emulator-5554 emulator-5556 emulator-5558 emulator-5590 emulator-5680 8B529975-BB5F-4ED5-8309-0FF04764BAE6"

# Exits (status 2) when any device named is protected. UDIDs compare in any case.
bench_refuse_protected() {
  local d p
  for d in "$@"; do
    for p in $BENCH_PROTECTED_BUILTIN ${BENCH_PROTECTED_DEVICES:-}; do
      [ "$(printf %s "$d" | tr '[:lower:]' '[:upper:]')" = "$(printf %s "$p" | tr '[:lower:]' '[:upper:]')" ] &&
        bench_die "$d is a protected device (built in, or in BENCH_PROTECTED_DEVICES); not touching it"
    done
  done
  return 0
}

# bench_refuse_protected_port PORT: exits (status 2) unless PORT is an even number whose console
# serial (emulator-PORT) and adb port's (emulator-PORT+1: the adb port is the next one up, and the
# console that owns that number would collide with it) are both unprotected. It looks at the
# number only, so a protected port is refused whether or not its device is up.
bench_refuse_protected_port() {
  local port=$1
  case $port in '' | *[!0-9]*) bench_die "port '$port' is not a number" ;; *) ;; esac
  { [ "${#port}" -le 5 ] && [ $((10#$port)) -le 65534 ]; } || bench_die "port $port is out of range"
  port=$((10#$port))
  [ $((port % 2)) = 0 ] || bench_die "port $port is odd: a console port is even (its adb port is the next one up)"
  bench_refuse_protected "emulator-$port" "emulator-$((port + 1))"
}

bench_help() {
  awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
}

bench_die() {
  echo "$(basename "$0"): $*" >&2
  exit 2
}
bench_log() { echo "[$(date +%H:%M:%S)] $*" >&2; }

# Options every benchmark takes. Call from the script's option loop with "$@"; on success it sets
# BENCH_SHIFT to the number of arguments it consumed.
bench_opt() {
  BENCH_SHIFT=1
  case $1 in
    -h | --help)
      bench_help
      exit 0
      ;;
    --rounds)
      [ $# -ge 2 ] || bench_die "--rounds needs a number"
      BENCH_ROUNDS=$2
      BENCH_SHIFT=2
      ;;
    --quick)
      BENCH_QUICK=1
      BENCH_ROUNDS=1
      ;;
    --out)
      [ $# -ge 2 ] || bench_die "--out needs a directory"
      BENCH_OUT=$2
      BENCH_SHIFT=2
      ;;
    *) bench_die "unknown option $1 (--help lists them)" ;;
  esac
}

# Exit status 3: this host can't run the benchmark (run-all.sh reports it as skipped).
bench_skip() {
  echo "$(basename "$0"): skipped: $*" >&2
  exit 3
}

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { awk -v a="$(now)" -v b="$1" -v d="${2:-2}" 'BEGIN { printf "%.*f\n", d, a - b }'; }
load1() {
  if [ -r /proc/loadavg ]; then cut -d' ' -f1 /proc/loadavg; else sysctl -n vm.loadavg | awk '{print $2}'; fi
}
ncpu() { getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu; }
median() {
  grep -E '^-?[0-9.]+$' | sort -n | awk '{ v[NR] = $1 } END { if (NR % 2) print v[(NR + 1) / 2]; else if (NR) printf "%.3f\n", (v[NR / 2] + v[NR / 2 + 1]) / 2 }'
}

# Starts the run: a work directory, the samples file, the host at the start, the exit trap.
bench_start() {
  BENCH_NAME=$1
  BENCH_WORK=$(mktemp -d "${TMPDIR:-/tmp}/bench-$BENCH_NAME.XXXXXX")
  BENCH_SAMPLES="$BENCH_WORK/samples.tsv"
  : >"$BENCH_SAMPLES"
  mkdir -p "$BENCH_OUT"
  python3 -I "$BENCH_DIR/summarize.py" host >"$BENCH_WORK/host-start.json"
  trap bench_cleanup EXIT
  trap 'exit 130' INT TERM
  bench_log "$BENCH_NAME: rounds=$BENCH_ROUNDS quick=$BENCH_QUICK work=$BENCH_WORK"
}

# bench_sample ARM METRIC VALUE: one measurement of round $BENCH_ROUND.
bench_sample() {
  printf '%s\t%s\t%s\t%s\t%s\n' "$BENCH_ROUND" "$1" "$2" "$3" "$(load1)" >>"$BENCH_SAMPLES"
  bench_log "round $BENCH_ROUND $1 $2=$3"
}

# Writes bench/results/<name>-<time>.json and prints the table. Optional: the arms' column order
# (comma-separated) and a note printed under the table.
bench_finish() {
  python3 -I "$BENCH_DIR/summarize.py" host >"$BENCH_WORK/host-end.json"
  local out
  out="$BENCH_OUT/$BENCH_NAME-$(date +%Y%m%d-%H%M%S).json"
  python3 -I "$BENCH_DIR/summarize.py" finish --name "$BENCH_NAME" --samples "$BENCH_SAMPLES" \
    --host-start "$BENCH_WORK/host-start.json" --host-end "$BENCH_WORK/host-end.json" --out "$out" \
    --args "$BENCH_ARGS" --arms "${1:-}" --note "${2:-}"
}

# Cleanup runs in reverse order of registration, after every tracked PID has stopped.
bench_on_exit() { BENCH_CLEANUP+=("$*"); }
bench_track() { BENCH_PIDS+=("$1"); }
bench_untrack() {
  local keep=() p
  for p in ${BENCH_PIDS[@]+"${BENCH_PIDS[@]}"}; do [ "$p" = "$1" ] || keep+=("$p"); done
  BENCH_PIDS=(${keep[@]+"${keep[@]}"})
}
# Stops a PID this run started: SIGINT, then SIGTERM after $2 s (30), then waits.
bench_stop_pid() {
  local pid=$1 grace=${2:-30} i
  kill -0 "$pid" 2>/dev/null || {
    bench_untrack "$pid"
    return 0
  }
  kill -INT "$pid" 2>/dev/null
  for i in $(seq 1 $((grace * 10))); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null
    for i in $(seq 1 100); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
  fi
  wait "$pid" 2>/dev/null
  bench_untrack "$pid"
}
bench_cleanup() {
  local rc=$? p i
  trap - EXIT INT TERM
  for p in ${BENCH_PIDS[@]+"${BENCH_PIDS[@]}"}; do bench_stop_pid "$p"; done
  for ((i = ${#BENCH_CLEANUP[@]} - 1; i >= 0; i--)); do eval "${BENCH_CLEANUP[i]}" || true; done
  [ -n "${BENCH_KEEP_WORK:-}" ] || rm -rf "${BENCH_WORK:-/nonexistent}"
  exit "$rc"
}

# CPU-seconds (user + system) a process and its descendants have used.
tree_pids() {
  local p c
  for p in "$@"; do
    echo "$p"
    for c in $(pgrep -P "$p" 2>/dev/null); do tree_pids "$c"; done
  done
}
cpu_s() {
  local pids
  pids=$(tree_pids "$@")
  if [ -r /proc/self/stat ]; then
    local tck
    tck=$(getconf CLK_TCK)
    for p in $pids; do [ -r "/proc/$p/stat" ] && sed 's/^.*) //' "/proc/$p/stat"; done |
      awk -v t="$tck" '{ s += $12 + $13 } END { printf "%.2f\n", s / t }'
  else
    # shellcheck disable=SC2046,SC2086
    ps -o time= $(printf -- '-p %s ' $pids) 2>/dev/null |
      awk '{ n = split($1, a, /[-:]/); s = 0; for (i = 1; i <= n; i++) s = s * 60 + a[i]; t += s } END { printf "%.2f\n", t }'
  fi
}
# Resident memory of a process and its descendants, MB.
rss_mb() {
  local pids
  pids=$(tree_pids "$@")
  # shellcheck disable=SC2046,SC2086
  ps -o rss= $(printf -- '-p %s ' $pids) 2>/dev/null | awk '{ s += $1 } END { printf "%d\n", s / 1024 }'
}

# --- Android -----------------------------------------------------------------------------------

android_env() {
  if [ -z "${ANDROID_HOME:-}" ]; then
    if [ "$(uname -s)" = Darwin ]; then ANDROID_HOME=$HOME/Library/Android/sdk; else ANDROID_HOME=$HOME/Android/Sdk; fi
  fi
  BENCH_SDK=$ANDROID_HOME
  BENCH_ADB="$BENCH_SDK/platform-tools/adb"
  BENCH_STOCK="$BENCH_SDK/emulator/emulator"
  BENCH_AVD_HOME=${ANDROID_AVD_HOME:-$HOME/.android/avd}
  [ -x "$BENCH_ADB" ] || bench_skip "no adb at $BENCH_ADB (emu -provision 36 installs platform-tools)"
}

# emu: $BENCH_EMU when given (--emu), else this checkout's release build, built and signed here.
emu_ready() {
  if [ -z "${BENCH_EMU:-}" ]; then
    bench_log "building target/release/emu"
    (cd "$BENCH_REPO" && cargo build -q --release -p simee-emu) || bench_die "cargo build --release -p simee-emu failed"
    BENCH_EMU="$BENCH_REPO/target/release/emu"
  fi
  if [ "$(uname -s)" = Darwin ]; then
    codesign -s - -f --entitlements "$BENCH_REPO/scripts/hypervisor.entitlements" "$BENCH_EMU" 2>&1 | grep -v 'replacing existing signature' || true
  fi
}

stock_ready() {
  [ -x "$BENCH_STOCK" ] || bench_skip "no SDK emulator at $BENCH_STOCK (the stock baseline)"
}

# image_dir DIR: sets IMAGE_DIR to a system image directory (relative to the SDK or absolute);
# skips the run when it isn't installed.
image_dir() {
  IMAGE_DIR=$1
  case $IMAGE_DIR in /*) ;; *) IMAGE_DIR="$BENCH_SDK/$IMAGE_DIR" ;; esac
  [ -f "$IMAGE_DIR/system.img" ] || bench_skip "no system image at $IMAGE_DIR (emu -provision <api> -tag <tag> installs one)"
}

# avd_create NAME SYSDIR [emu -create-avd options]: refuses a name that exists; deleted on exit.
# The AVD starts with no userdata, so its first boot is a real first boot: where emu's
# -create-avd would otherwise start it from a settled golden userdata, -no-golden is passed.
# AVD_MODE says which ran ("fresh, -no-golden", or "fresh" for an emu without golden userdata);
# the scripts put it in their notes.
avd_create() {
  local name=$1 sysdir=$2 nog=()
  shift 2
  [ -e "$BENCH_AVD_HOME/$name.avd" ] && bench_die "AVD $name already exists; not touching it"
  if "$BENCH_EMU" -help 2>&1 | grep -q -- '-no-golden'; then
    nog=(-no-golden)
    AVD_MODE="fresh, -no-golden"
  else AVD_MODE=fresh; fi
  "$BENCH_EMU" -create-avd "$name" -sysdir "$sysdir" ${nog[@]+"${nog[@]}"} "$@" >"$BENCH_WORK/create-$name.log" 2>&1 ||
    {
      cat "$BENCH_WORK/create-$name.log" >&2
      bench_die "emu -create-avd $name failed"
    }
  bench_on_exit avd_delete "$name"
  bench_log "created AVD $name ($AVD_MODE)"
}
avd_delete() {
  local i
  [ -e "$BENCH_AVD_HOME/$1.avd" ] || [ -e "$BENCH_AVD_HOME/$1.ini" ] || return 0
  [ -n "${BENCH_KEEP_DEVICES:-}" ] && {
    bench_log "kept AVD $1"
    return 0
  }
  for i in 1 2 3 4 5; do
    "$BENCH_EMU" -delete-avd "$1" >/dev/null 2>&1 && {
      bench_log "deleted AVD $1"
      return 0
    }
    sleep 2
  done
  echo "could not delete AVD $1: $BENCH_EMU -delete-avd $1" >&2
}

# A port no emulator listens on (console N, adb N+1).
port_free() {
  local p
  for p in "$1" $(($1 + 1)); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then return 1; fi
  done
  return 0
}
android_port() {
  port_free "$1" || bench_die "port $1 or $(($1 + 1)) is in use: pass --port with a free even port"
}

# android_boot ARM AVD PORT LOG [emulator args]: starts emu or the SDK emulator and waits for
# sys.boot_completed (BOOT_TIMEOUT s, default 600). ARM is emu, stock, or emu:K=V,K=V (env for an
# A/B arm). Always -no-window -no-audio -no-boot-anim, and -no-snapshot-save unless SNAPSHOT_SAVE=1.
# Sets BOOT_PID, BOOT_S (launch to sys.boot_completed), ADB_S (launch to adb listing the
# device), PEAK_RSS_MB; returns 1 when it didn't boot. The environment is stripped as
# scripts/android-bench.sh strips it (no Java on PATH).
android_boot() {
  local arm=$1 avd=$2 port=$3 log=$4 bin t0 deadline serial="emulator-$3" tick=0 r
  shift 4
  local envs=() own=()
  case $arm in
    stock*) bin=$BENCH_STOCK ;;
    emu*)
      bin=$BENCH_EMU
      own=(-verbose)
      ;;
    *) bench_die "arm $arm: expected emu, stock or emu:K=V" ;;
  esac
  # The SDK emulator saves a quickboot snapshot on exit unless told not to; SNAPSHOT_SAVE=1 lets it.
  [ "${SNAPSHOT_SAVE:-0}" = 1 ] || own+=(-no-snapshot-save)
  case $arm in *:*) IFS=, read -r -a envs <<<"${arm#*:}" ;; *) ;; esac
  t0=$(now)
  env -i HOME="$HOME" USER="${USER:-$(id -un)}" TMPDIR="${TMPDIR:-/tmp}" PATH="/usr/bin:/bin:/usr/sbin:/sbin" \
    ANDROID_HOME="$BENCH_SDK" ANDROID_SDK_ROOT="$BENCH_SDK" ${ANDROID_AVD_HOME:+ANDROID_AVD_HOME="$ANDROID_AVD_HOME"} \
    ${envs[@]+"${envs[@]}"} "$bin" -avd "$avd" -port "$port" -no-window -no-audio -no-boot-anim \
    ${own[@]+"${own[@]}"} "$@" >"$log" 2>&1 &
  BOOT_PID=$!
  bench_track "$BOOT_PID"
  BOOT_S=""
  ADB_S=""
  PEAK_RSS_MB=0
  deadline=$((SECONDS + ${BOOT_TIMEOUT:-600}))
  while [ "$SECONDS" -lt "$deadline" ]; do
    kill -0 "$BOOT_PID" 2>/dev/null || {
      bench_log "$arm exited; see $log"
      tail -5 "$log" >&2
      return 1
    }
    tick=$((tick + 1))
    if [ $((tick % 4)) -eq 1 ]; then
      r=$(rss_mb "$BOOT_PID")
      [ "${r:-0}" -gt "$PEAK_RSS_MB" ] && PEAK_RSS_MB=$r
    fi
    [ -z "$ADB_S" ] && "$BENCH_ADB" -s "$serial" get-state 2>/dev/null | grep -q device && ADB_S=$(since "$t0")
    if [ -n "$ADB_S" ] && [ "$("$BENCH_ADB" -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; then
      BOOT_S=$(since "$t0")
      return 0
    fi
    sleep 0.25
  done
  bench_log "$arm: no sys.boot_completed in ${BOOT_TIMEOUT:-600} s"
  return 1
}
# Stops the device android_boot started: the console's kill, then SIGINT/SIGTERM by PID.
android_stop() {
  local pid=$1 port=$2 i
  "$BENCH_ADB" -s "emulator-$port" emu kill >/dev/null 2>&1
  for i in $(seq 1 150); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  bench_stop_pid "$pid"
}
# Seconds from now until the launcher has focus (dumpsys window), polled each second, or "never".
launcher_s() {
  local serial=$1 t0=$2 deadline=$((SECONDS + ${3:-120}))
  while [ "$SECONDS" -lt "$deadline" ]; do
    "$BENCH_ADB" -s "$serial" shell dumpsys window 2>/dev/null | grep mCurrentFocus | grep -qi launcher && {
      since "$t0" 1
      return
    }
    sleep 1
  done
  echo never
}

# --- iOS ---------------------------------------------------------------------------------------

ios_env() {
  [ "$(uname -s)" = Darwin ] || bench_skip "iOS needs macOS with Xcode"
  xcrun simctl help >/dev/null 2>&1 || bench_skip "no simctl (Xcode with a simulator runtime)"
}
# simee: $BENCH_SIMEE when given (--simee), else this checkout's release build.
simee_ready() {
  if [ -z "${BENCH_SIMEE:-}" ]; then
    bench_log "building target/release/simee"
    (cd "$BENCH_REPO" && cargo build -q --release -p simee) || bench_die "cargo build --release -p simee failed"
    BENCH_SIMEE="$BENCH_REPO/target/release/simee"
  fi
  [ -x "$BENCH_SIMEE" ] || bench_die "no simee at $BENCH_SIMEE"
}
# The newest available iOS runtime's identifier.
ios_runtime() {
  xcrun simctl list runtimes -j | python3 -I -c '
import json, sys
rs = [r for r in json.load(sys.stdin)["runtimes"] if r.get("isAvailable") and r.get("platform", "iOS") == "iOS" and ".iOS-" in r["identifier"]]
rs.sort(key=lambda r: [int(x) for x in r["version"].split(".")])
print(rs[-1]["identifier"] if rs else "")'
}
# ios_create NAME TYPE RUNTIME: a new simulator (simctl create), its UDID in IOS_UDID; deleted on
# exit. (Not in a command substitution: the cleanup has to register in this shell.)
ios_create() {
  IOS_UDID=$(xcrun simctl create "$1" "$2" "$3") || bench_die "simctl create $1 '$2' $3 failed"
  bench_on_exit ios_delete "$IOS_UDID"
  bench_log "created simulator $1 $IOS_UDID"
}
ios_delete() {
  xcrun simctl list devices | grep -qF "($1)" || return 0
  [ -n "${BENCH_KEEP_DEVICES:-}" ] && {
    bench_log "kept simulator $1"
    return 0
  }
  xcrun simctl shutdown "$1" >/dev/null 2>&1
  xcrun simctl delete "$1" && bench_log "deleted simulator $1"
}
# Shut down one of this run's simulators and wait until it says so.
ios_shutdown() {
  local i
  xcrun simctl shutdown "$1" >/dev/null 2>&1
  for i in $(seq 1 60); do
    xcrun simctl list devices | grep -F "$1" | grep -q '(Shutdown)' && return 0
    sleep 1
  done
  return 1
}
# serve_start SIMEE SOCKET: `simee serve` on SOCKET, tracked; returns once the socket exists.
serve_start() {
  local i
  SIMEE_SOCKET=$2 "$1" serve >"$BENCH_WORK/serve-$$-$RANDOM.log" 2>&1 &
  SERVE_PID=$!
  bench_track "$SERVE_PID"
  for i in $(seq 1 100); do
    [ -S "$2" ] && return 0
    sleep 0.1
  done
  bench_die "simee serve didn't open $2"
}
# SOCK_DIR: a short directory for unix sockets (the 104-byte sun_path limit rules out a long
# TMPDIR), removed on exit.
sock_dir() {
  SOCK_DIR=$(mktemp -d /tmp/sb.XXXXXX)
  bench_on_exit rm -rf "$SOCK_DIR"
}

# cpu_timed CMD...: runs CMD (output to $BENCH_WORK/cpu_timed.log) and prints "wall_s cpu_s rc",
# where cpu_s is the user + system time of CMD and every child it waited for (getrusage), which
# other load on the host doesn't inflate.
cpu_timed() {
  python3 -I -c '
import resource, subprocess, sys, time
t = time.monotonic()
with open(sys.argv[1], "w") as log:
    r = subprocess.run(sys.argv[2:], stdout=log, stderr=subprocess.STDOUT)
u = resource.getrusage(resource.RUSAGE_CHILDREN)
print(f"{time.monotonic() - t:.1f} {u.ru_utime + u.ru_stime:.1f} {r.returncode}")' "$BENCH_WORK/cpu_timed.log" "$@"
}
