#!/usr/bin/env bash
# Linux with KVM (GitHub's x86_64 Ubuntu runner): boot, memory, idle CPU, screenshot, frame pacing.
#
#   scripts/bench/linux-kvm.sh [--image DIR] [--lcd WxH@DPI] [--port N] [--settle S] [--window S]
#                              [--shots N] [--dry-run] [--no-quiet-gate] [--rounds N] [--quick] [--out DIR]
#
# One new AVD per round (emu -create-avd -no-golden, 4 vCPUs, 2 GB), booted with `-accel on` (KVM or
# fail, so a run that fell back to TCG is an error, never a slow number), then:
#   cold boot    the AVD's first boot: seconds until adb lists the device and until
#                sys.boot_completed, peak RSS of the process tree
#   warm boot    a second boot of the same AVD, the same two times, then, once --settle s (60) have
#                passed:
#   memory       RSS and PSS of emu and everything under it (/proc/<pid>/smaps_rollup), MB
#   idle CPU     the tree's user + system CPU over the next --window s (60), as % of one core
#   screenshot   --shots (30) `simee screenshot` calls, each a process of its own through the
#                display socket: p50 and p95 in ms; the same for `adb exec-out screencap -p`
#   frame pacing scripts/android-jank.sh: 10 launcher app-drawer swipes up and down, then the
#                launcher's `dumpsys gfxinfo`: janky frames %, frame time p50/p90/p99
# then deletes the AVD. --rounds defaults to 5 here (lib.sh's default is 3). The runner's host
# profile (lscpu, memory, /dev/kvm, load) is recorded before anything is measured and merged into
# the result JSON under "runner". Also written beside it: linux-kvm-summary.md, the table.
#
# --dry-run prints the plan (what would run, with which ports, image and timings) and exits before
# building or booting anything. --no-quiet-gate skips the quiet-host gate (scripts/bench/quiet.sh,
# when it exists): a hosted runner is never idle, so the workflow passes it and the load is
# recorded instead; these numbers are labelled "GitHub runner", never mixed into a quiet-host row.
#
# Defaults: --image system-images/android-36/default/x86_64 (emu -provision 36 -tag default -abi
# x86_64), --lcd 720x1280@320, --port 5700 (adb 5701; ports 5700 and up). Needs an x86_64 Linux host
# with a usable /dev/kvm, QEMU 11 or newer ($EMU_QEMU) and the renderer, as
# .github/workflows/software-mode.yml sets them up. Run by .github/workflows/bench-linux.yml.
# Reproduces docs/research/linux-kvm-bench.md.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
image=system-images/android-36/default/x86_64
lcd=720x1280@320
port=5700
settle=60
window=60
shots=30
dry=0
gate=1
BENCH_ROUNDS=5
while [ $# -gt 0 ]; do
  case $1 in
    --image)
      image=$2
      shift 2
      ;;
    --lcd)
      lcd=$2
      shift 2
      ;;
    --port)
      port=$2
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
    --dry-run)
      dry=1
      shift
      ;;
    --no-quiet-gate)
      # Handled here so the script also runs on a lib.sh that doesn't know the option.
      gate=0
      shift
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
if [ "$BENCH_QUICK" = 1 ]; then
  settle=10
  window=10
  shots=5
fi
for v in port settle window shots; do
  case ${!v} in '' | *[!0-9]*) bench_die "--$v needs a number" ;; *) ;; esac
done
[ "$port" -ge 5700 ] || bench_die "--port $port: benchmarks use ports 5700 and up"
bench_refuse_protected_port "$port"
# shellcheck disable=SC2034 # read by lib.sh
[ "$gate" = 1 ] || BENCH_QUIET_GATE=0

if [ "$dry" = 1 ]; then
  cat <<EOF
linux-kvm.sh plan (dry run: nothing is built, created or booted)
  image         $image
  AVD           bench-kvm-<pid>-<round>: emu -create-avd -no-golden -cores 4 -memory 2048 -lcd $lcd
  ports         console $port, adb $((port + 1))
  rounds        $BENCH_ROUNDS
  accel         -accel on (KVM or fail)
  quiet gate    $([ "$gate" = 1 ] && echo "on (scripts/bench/quiet.sh, when present)" || echo "off (--no-quiet-gate): load is recorded, not gated")
  each round    1. cold boot to sys.boot_completed (adb listing time, peak RSS)
                2. stop, warm boot of the same AVD (same two times)
                3. settle ${settle} s, then RSS and PSS of the process tree
                4. idle host CPU over ${window} s
                5. ${shots} x simee screenshot, ${shots} x adb screencap: p50, p95
                6. scripts/android-jank.sh: janky %, frame time p50/p90/p99
                7. stop (console kill, then SIGINT/SIGTERM by recorded PID), delete the AVD
  results       $BENCH_OUT/linux-kvm-<time>.json (+ linux-kvm-summary.md)
  runner profile  $(uname -m) $(getconf _NPROCESSORS_ONLN) CPUs, load $(load1), /dev/kvm $([ -e /dev/kvm ] && echo present || echo absent)
EOF
  exit 0
fi

[ "$(uname -s)" = Linux ] || bench_skip "needs Linux with /dev/kvm"
if ! { [ -r /dev/kvm ] && [ -w /dev/kvm ]; }; then bench_skip "no usable /dev/kvm (the workflow's enable-kvm step opens it)"; fi
android_env
image_dir "$image"
android_port "$port"
bench_start linux-kvm
emu_ready
simee_ready

# The runner's profile, before the first measurement. Everything is read from /proc, /sys and lscpu.
profile="$BENCH_WORK/runner.json"
python3 -I - "$profile" <<'EOF'
import json, os, subprocess, sys

def sh(*cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=20).stdout
    except (OSError, subprocess.SubprocessError):
        return ''

lscpu = {}
for line in sh('lscpu').splitlines():
    k, _, v = line.partition(':')
    lscpu[k.strip()] = v.strip()
mem = {}
for line in open('/proc/meminfo'):
    k, _, v = line.partition(':')
    mem[k] = int(v.split()[0])
kvm = os.path.exists('/dev/kvm')
p = {
    'runner_name': os.environ.get('RUNNER_NAME', ''),
    'runner_image': os.environ.get('ImageVersion', ''),
    'github_run_url': (f"{os.environ['GITHUB_SERVER_URL']}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
                       if os.environ.get('GITHUB_RUN_ID') else ''),
    'cpu_model': lscpu.get('Model name', ''),
    'arch': lscpu.get('Architecture', ''),
    'cpus': os.cpu_count(),
    'threads_per_core': lscpu.get('Thread(s) per core', ''),
    'hypervisor_vendor': lscpu.get('Hypervisor vendor', ''),
    'mem_total_gb': round(mem.get('MemTotal', 0) / 2**20, 1),
    'mem_available_gb': round(mem.get('MemAvailable', 0) / 2**20, 1),
    'kvm_device': kvm,
    'kernel': os.uname().release,
    'load_avg': open('/proc/loadavg').read().split()[:3],
    'top_cpu_processes': sh('ps', '-eo', 'pcpu,comm', '--sort=-pcpu').splitlines()[1:6],
}
json.dump(p, open(sys.argv[1], 'w'), indent=1)
print(json.dumps(p))
EOF
bench_log "runner profile recorded (load $(load1))"

label=emu-kvm
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
# pss_mb PID...: proportional set size of each process and its descendants, MB.
pss_mb() {
  local p
  for p in $(tree_pids "$@"); do
    [ -r "/proc/$p/smaps_rollup" ] && awk '/^Pss:/ { s += $2 } END { print s + 0 }' "/proc/$p/smaps_rollup"
  done | awk '{ s += $1 } END { printf "%d\n", s / 1024 }'
}
# pct P: the P-th percentile (linear interpolation) of the numbers on stdin, one per line.
pct() {
  sort -n | awk -v p="$1" '{ v[NR] = $1 } END { if (!NR) exit; r = (NR - 1) * p / 100; lo = int(r); hi = (lo + 1 < NR) ? lo + 1 : lo; printf "%.1f\n", v[lo + 1] + (v[hi + 1] - v[lo + 1]) * (r - lo) }'
}
# time_calls N CMD...: runs CMD N times (output discarded) and prints each call's wall ms, one per line.
time_calls() {
  local n=$1 i t0
  shift
  for i in $(seq 1 "$n"); do
    t0=$(now)
    "$@" >/dev/null 2>&1 || {
      echo "$*: failed" >&2
      continue
    }
    since "$t0" 4 | awk '{ printf "%.1f\n", $1 * 1000 }'
  done
}
# One boot, with `-accel on`; records its two times and peak RSS under PHASE. Returns 1 on no boot.
boot() {
  local avd=$1 phase=$2
  if ! android_boot emu "$avd" "$port" "$BENCH_WORK/$phase-$BENCH_ROUND.log" -accel on; then
    bench_sample "$label" "$phase boot: launch until sys.boot_completed, s" failed
    android_stop "$BOOT_PID" "$port"
    return 1
  fi
  grep -q 'guest CPU on KVM' "$BENCH_WORK/$phase-$BENCH_ROUND.log" || bench_log "round $BENCH_ROUND $phase: no 'guest CPU on KVM' line in the log"
  bench_sample "$label" "$phase boot: launch until sys.boot_completed, s" "$BOOT_S"
  bench_sample "$label" "$phase boot: launch until adb lists the device, s" "$ADB_S"
  bench_sample "$label" "$phase boot: peak RSS of the process tree, MB" "$PEAK_RSS_MB"
  return 0
}

for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  avd="bench-kvm-$$-$BENCH_ROUND"
  avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory 2048 -lcd "$lcd"
  if boot "$avd" cold; then android_stop "$BOOT_PID" "$port"; fi
  boot "$avd" warm || continue
  bench_log "round $BENCH_ROUND: settling ${settle} s"
  sleep "$settle"
  bench_sample "$label" "memory: RSS of the process tree, MB" "$(rss_mb "$BOOT_PID")"
  bench_sample "$label" "memory: PSS of the process tree, MB" "$(pss_mb "$BOOT_PID")"
  # Idle host CPU: CPU seconds of the tree over the window, as % of one core (wall time from /proc/uptime).
  c0=$(cpu_s "$BOOT_PID")
  u0=$(cut -d' ' -f1 /proc/uptime)
  sleep "$window"
  c1=$(cpu_s "$BOOT_PID")
  u1=$(cut -d' ' -f1 /proc/uptime)
  bench_sample "$label" "idle host CPU over ${window} s, % of one core" \
    "$(awk -v a="$c0" -v b="$c1" -v t0="$u0" -v t1="$u1" 'BEGIN { printf "%.1f", (b - a) * 100 / (t1 - t0) }')"
  # A first call warms the path (the server's first frame); it isn't counted.
  "$BENCH_SIMEE" screenshot "$avd" "$BENCH_WORK/shot.png" >/dev/null 2>&1
  time_calls "$shots" "$BENCH_SIMEE" screenshot "$avd" "$BENCH_WORK/shot.png" >"$BENCH_WORK/shots-simee.txt"
  time_calls "$shots" "$BENCH_ADB" -s "emulator-$port" exec-out screencap -p >"$BENCH_WORK/shots-adb.txt"
  for kind in simee adb; do
    n=$(wc -l <"$BENCH_WORK/shots-$kind.txt")
    if [ "$n" -gt 0 ]; then
      bench_sample "$label" "screenshot via $kind: p50, ms" "$(pct 50 <"$BENCH_WORK/shots-$kind.txt")"
      bench_sample "$label" "screenshot via $kind: p95, ms" "$(pct 95 <"$BENCH_WORK/shots-$kind.txt")"
    else
      bench_sample "$label" "screenshot via $kind: p50, ms" failed
    fi
  done
  jank=$(ANDROID_HOME=$BENCH_SDK "$BENCH_REPO/scripts/android-jank.sh" "emulator-$port" "$BOOT_PID" 2>/dev/null | tail -1)
  bench_log "round $BENCH_ROUND jank: $jank"
  bench_sample "$label" "janky frames, %" "$(field janky_pct "$jank")"
  bench_sample "$label" "frame time p50, ms" "$(field p50 "$jank")"
  bench_sample "$label" "frame time p90, ms" "$(field p90 "$jank")"
  bench_sample "$label" "frame time p99, ms" "$(field p99 "$jank")"
  bench_sample "$label" "frames rendered" "$(field frames "$jank")"
  android_stop "$BOOT_PID" "$port"
  avd_delete "$avd"
done

table=$(bench_finish "$label" "GitHub runner, Linux with KVM (-accel on); image $IMAGE_DIR (AVD created ${AVD_MODE:-fresh}), $lcd, 4 vCPUs, 2 GB; ${settle} s settle, ${window} s idle window, ${shots} screenshots per round. Not comparable with a quiet-host row.")
printf '%s\n' "$table"
result=""
for f in "$BENCH_OUT"/linux-kvm-*.json; do
  [ -e "$f" ] && case $f in *.host.json) ;; *) [ -z "$result" ] || [ "$f" -nt "$result" ] && result=$f ;; esac
done
if [ -n "$result" ]; then
  python3 -I - "$result" "$profile" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
r['runner'] = json.load(open(sys.argv[2]))
json.dump(r, open(sys.argv[1], 'w'), indent=1)
EOF
  {
    echo "### Linux with KVM (GitHub runner)"
    echo
    printf '%s\n' "$table"
  } >"$BENCH_OUT/linux-kvm-summary.md"
  bench_log "result: $result"
fi
