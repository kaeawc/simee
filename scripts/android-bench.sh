#!/usr/bin/env bash
# Boot one AVD and measure it, the same way for simee and the stock emulator, so results compare.
#
#   scripts/android-bench.sh <label> <avd> [emu|stock] [extra args...]   -> one key=value line, also in bench/<label>.result
#
# Metrics: seconds to `adb device` and to sys.boot_completed; median-of-3 latency of common adb
# shell calls; guest CPU microbenchmarks; the emulator process's peak RSS. Runs with a stripped
# environment (no Java on PATH). Stops the instance it started, by PID.
#
# Telling a starved run from a slow emulator: `cpu_s` is the emulator's CPU-seconds (user + sys,
# whole process tree) at boot, `vcpu0_runs` (emu only, from its -verbose line at boot) how many
# times vCPU0 entered the guest by then, and `load1_start`/`load1_boot` the host's 1-minute load
# at launch and at boot. A host with more runnable threads than CPUs starves the guest's vCPU
# threads: `starved=yes` when either load is above the CPU count. Keep those runs out of medians.
# `stop_s` is seconds from the console's `kill` to the process exiting (`stop_overran=yes` when
# emu printed that its shutdown overran and it exited anyway).
set -uo pipefail
label=${1:?usage: scripts/android-bench.sh <label> <avd> [emu|stock] [args...]}
avd=${2:?avd}; kind=${3:-emu}; shift $(( $# >= 3 ? 3 : $# ))
repo=$(cd "$(dirname "$0")/.." && pwd)
sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
adb="$sdk/platform-tools/adb"
out="$repo/bench"; mkdir -p "$out"
port=${PORT:-5600}; serial="emulator-$port"
own=()
case "$kind" in
  emu) (cd "$repo" && cargo build -q --release -p simee-emu) || exit 1
       [ "$(uname -s)" = Darwin ] && codesign -s - -f --entitlements "$repo/scripts/hypervisor.entitlements" "$repo/target/release/emu" 2>/dev/null
       bin="$repo/target/release/emu"; own=(-verbose) ;;
  stock) bin="$sdk/emulator/emulator" ;;
  *) echo "kind must be emu or stock" >&2; exit 2 ;;
esac
# Wall-clock seconds with sub-second precision. One perl start (a few ms, no python) per reading,
# and readings are taken only at events (start, adb up, boot), never once per poll.
now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { awk -v a="$(now)" -v b="$1" -v d="${2:-1}" 'BEGIN { printf "%.*f\n", d, a - b }'; }
load1() { if [ -r /proc/loadavg ]; then cut -d' ' -f1 /proc/loadavg; else sysctl -n vm.loadavg | awk '{print $2}'; fi; }
ncpu=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu)
l0=$(load1)
t0=$(now)
# emu's own switches (EMU_RENDERER_GL=1, EMU_GUEST_EGL=...) pass through, for A/B runs.
emu_env=()
while IFS= read -r kv; do emu_env+=("$kv"); done < <(env | grep '^EMU_' || true)
env -i HOME="$HOME" USER="${USER:-$(id -un)}" TMPDIR="${TMPDIR:-/tmp}" PATH="/usr/bin:/bin:/usr/sbin:/sbin" ANDROID_HOME="$sdk" ANDROID_SDK_ROOT="$sdk" \
  ${ANDROID_AVD_HOME:+ANDROID_AVD_HOME="$ANDROID_AVD_HOME"} ${emu_env[@]+"${emu_env[@]}"} \
  "$bin" -avd "$avd" -port "$port" -no-window -no-audio -no-boot-anim -no-snapshot-save ${own[@]+"${own[@]}"} "$@" > "$out/$label.log" 2>&1 &
pid=$!
# CPU-seconds of the emulator's process tree (the stock launcher execs a qemu child).
cpu_s() { ps -o time= -p "$pid" $(pgrep -P "$pid") 2>/dev/null | awk -F: '{ s += (NF == 3) ? $1*3600 + $2*60 + $3 : $1*60 + $2 } END { printf "%.1f", s }'; }
deadline=$(( SECONDS + ${BOOT_TIMEOUT:-600} )); state=timeout; t_adb=""; t_line=""; rss=0; tick=0
# Poll every POLL seconds (default 0.15), so a sub-second win shows. Per poll: a builtin liveness
# check and a grep of emu's log for its own `emu: boot completed in X s` line (emu only); adb is
# asked about the device until it's up, and about sys.boot_completed on each poll for the stock
# emulator (which prints nothing) or once the log line has appeared (to confirm it). Peak RSS is
# sampled about once a second, not every poll.
while [ "$SECONDS" -lt "$deadline" ]; do
  kill -0 $pid 2>/dev/null || { state=exited; break; }
  tick=$((tick + 1))
  if [ $((tick % 7)) -eq 1 ]; then
    r=$(ps -o rss= -p $pid $(pgrep -P $pid) 2>/dev/null | awk '{s+=$1} END{print s+0}'); [ "$r" -gt "$rss" ] && rss=$r
  fi
  [ -z "$t_adb" ] && "$adb" -s "$serial" get-state 2>/dev/null | grep -q device && t_adb=$(since "$t0")
  if [ "$kind" = stock ] || [ -n "$t_line" ] || [ $((tick % 14)) -eq 0 ] || { grep -q '^emu: boot completed' "$out/$label.log" 2>/dev/null && t_line=$(since "$t0" 2); }; then
    [ "$("$adb" -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ] && { state=booted; break; }
  fi
  sleep "${POLL:-0.15}"
done
t_boot=$(since "$t0")
cpu_boot=$(cpu_s); l1=$(load1)
# emu's own figure (seconds from its start to the guest's boot event), when it printed one.
emu_s=$(sed -n 's/^emu: boot completed in \([0-9.]*\)s.*/\1/p' "$out/$label.log" 2>/dev/null | head -1)
# vCPU0's guest entries at boot: the sum of its exit counters on emu's -verbose line.
runs0=$(sed -n 's/^emu: at boot completed: *vcpu0: //p' "$out/$label.log" 2>/dev/null | head -1 | tr ' ' '\n' | awk -F= 'NF == 2 { s += $2; n++ } END { if (n) print s }')
starved=$(awk -v a="$l0" -v b="$l1" -v n="$ncpu" 'BEGIN { print (a > n || b > n) ? "yes" : "no" }')
lat() { local s; s=$(now); "$adb" -s "$serial" shell "$@" >/dev/null 2>&1; since "$s" 2; }
med3() { printf '%s\n%s\n%s\n' "$(lat "$@")" "$(lat "$@")" "$(lat "$@")" | sort -n | sed -n 2p; }
m=""
if [ -n "$t_adb" ]; then
  m="echo=$(med3 echo hi) getprop=$(med3 getprop ro.build.version.sdk)"
  m+=" loop100k=$(lat 'i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done')"
  m+=" md5x4=$(lat 'for j in 1 2 3 4; do (dd if=/dev/zero bs=1048576 count=64 2>/dev/null | md5sum >/dev/null) & done; wait')"
  [ "$state" = booted ] && m+=" pm_list=$(med3 pm list packages) am_start_W=$(med3 am start -W -n com.android.settings/.Settings)"
fi
# Stop: the console's kill, then SIGTERM by PID if it is still up after STOP_WAIT seconds (15).
ts=$(now); "$adb" -s "$serial" emu kill >/dev/null 2>&1
stop_by=$(( SECONDS + ${STOP_WAIT:-15} ))
while kill -0 $pid 2>/dev/null && [ "$SECONDS" -lt "$stop_by" ]; do sleep 0.1; done
t_stop=$(since "$ts")
kill $pid 2>/dev/null; wait $pid 2>/dev/null
overran=no; grep -q '^emu: shutdown took over' "$out/$label.log" 2>/dev/null && overran=yes
line="label=$label kind=$kind state=$state adb_s=${t_adb:-} boot_s=$t_boot emu_boot_s=${emu_s:-} log_line_s=${t_line:-} peak_rss_mb=$((rss / 1024))"
line+=" cpu_s=$cpu_boot vcpu0_runs=${runs0:-} load1_start=$l0 load1_boot=$l1 starved=$starved $m stop_s=$t_stop stop_overran=$overran"
echo "$line" | tee "$out/$label.result"
exit 0
