#!/usr/bin/env bash
# Tests for the quiet-host gate and the host record, with no device and no load needed: a fake
# /proc and /sys (BENCH_QUIET_PROC, BENCH_QUIET_SYS) and stubs for ps, sysctl, pmset, powermetrics,
# sudo, id and getconf on PATH stand in for the host, on Linux and (through BENCH_QUIET_OS=Darwin
# and the stubs) macOS. Covered:
#   - a quiet host passes; a loaded one is refused (status 4) with the load named;
#   - a foreign emulator, simulator or compiler is refused by name and PID, while this shell, its
#     ancestors, its descendants and BENCH_QUIET_ALLOW_PIDS are not, and a sibling of an ancestor is;
#   - thermal pressure and battery power, on Linux (/sys) and on macOS (pmset, powermetrics);
#   - a bounded wait: a host that quietens within it passes, one that doesn't is refused in time;
#   - the record: well-formed JSON with the fields the issue lists, beside each result, and
#     marked unpublishable when the gate was skipped, the run was quick or a measure is noisy;
#   - p50, p95 and IQR, the noise budgets, `summarize.py check`, and run-all.sh's --quiet.
#
#   scripts/bench/test-quiet.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/test-quiet.XXXXXX")
trap 'rm -rf "$work"' EXIT
fails=0
check() { # description, condition result
  if [ "$2" = ok ]; then echo "ok   $1"; else
    echo "FAIL $1"
    fails=$((fails + 1))
  fi
}
has() { grep -qF -- "$1" <<<"$2"; }
hasnt() { ! grep -qF -- "$1" <<<"$2"; }

# --- the fake host -----------------------------------------------------------------------------
real_getconf=$(command -v getconf)
real_id=$(command -v id)
mkdir -p "$work/bin" "$work/proc" "$work/sys"
stub() { # name, body
  printf '#!/usr/bin/env bash\n%s\n' "$2" >"$work/bin/$1"
  chmod +x "$work/bin/$1"
}
# shellcheck disable=SC2016
stub ps 'cat "$FAKE_PS"'
# shellcheck disable=SC2016 # the stubs expand their own variables when they run
stub getconf '[ "${1:-}" = _NPROCESSORS_ONLN ] && { echo "${FAKE_NCPU:-8}"; exit 0; }; exec '"$real_getconf"' "$@"'
# shellcheck disable=SC2016
stub sysctl 'case $2 in vm.loadavg) cat "$FAKE_LOADAVG" ;; hw.ncpu) echo "${FAKE_NCPU:-8}" ;; *) exit 1 ;; esac'
# shellcheck disable=SC2016
stub pmset 'case "$*" in "-g therm") cat "$FAKE_THERM" ;; "-g batt") cat "$FAKE_BATT" ;; *) exit 1 ;; esac'
# shellcheck disable=SC2016
stub id '[ "${1:-}" = -u ] && { echo "${FAKE_UID:-1000}"; exit 0; }; exec '"$real_id"' "$@"'
stub sudo 'exit 1'
# shellcheck disable=SC2016
stub powermetrics 'cat "$FAKE_PM"'
export PATH="$work/bin:$PATH"
export FAKE_PS="$work/ps.txt" FAKE_LOADAVG="$work/loadavg.mac" FAKE_THERM="$work/therm.txt"
export FAKE_BATT="$work/batt.txt" FAKE_PM="$work/pm.txt"
export BENCH_QUIET_PROC="$work/proc" BENCH_QUIET_SYS="$work/sys" BENCH_QUIET_SELF_PID=100
export BENCH_QUIET_WAIT=1 BENCH_QUIET_POLL=0.2

reset_host() { # a quiet Linux host with 8 CPUs, no sensors, no battery
  rm -rf "$work/sys" "$work/proc"
  mkdir -p "$work/sys/class" "$work/proc"
  echo "0.20 0.30 0.40 1/200 999" >"$work/proc/loadavg"
  cat >"$FAKE_PS" <<'EOF'
    1     0 init
   10     1 sshd
   50    10 bash
  100    50 quiet.sh
EOF
  export BENCH_QUIET_OS=Linux FAKE_NCPU=8 FAKE_UID=1000
  unset BENCH_QUIET_ALLOW_PIDS BENCH_QUIET_MAX_LOAD BENCH_QUIET_LOAD_FACTOR
}
zone() { # type temp-in-millidegrees
  mkdir -p "$work/sys/class/thermal/thermal_zone0"
  echo "$1" >"$work/sys/class/thermal/thermal_zone0/type"
  echo "$2" >"$work/sys/class/thermal/thermal_zone0/temp"
}
supply() { # name type [key value]...
  local d="$work/sys/class/power_supply/$1"
  mkdir -p "$d"
  echo "$2" >"$d/type"
  shift 2
  while [ $# -ge 2 ]; do
    echo "$2" >"$d/$1"
    shift 2
  done
}
gate() { # runs quiet.sh gate; sets out, err, rc
  out=$("$here/quiet.sh" gate 2>"$work/err")
  rc=$?
  err=$(cat "$work/err")
}
reasons() { "$here/quiet.sh" check; }

# --- Linux -------------------------------------------------------------------------------------
reset_host
gate
check "a quiet Linux host passes" "$([ "$rc" = 0 ] && [ "$out" = passed ] && echo ok)"

echo "13.40 12.00 9.00 4/900 999" >"$work/proc/loadavg"
gate
check "a loaded host is refused with status 4" "$([ "$rc" = 4 ] && echo ok)"
check "the refusal names the load and its limit" "$(has 'the 1-minute load is 13.40, not below 4.00' "$err" && has 'refused: this host is not quiet' "$err" && echo ok)"
check "the refusal says how to run anyway" "$(has -- '--no-quiet-gate' "$err" && echo ok)"
out=$(BENCH_QUIET_MAX_LOAD=20 "$here/quiet.sh" gate 2>/dev/null)
rc=$?
check "BENCH_QUIET_MAX_LOAD loosens the limit" "$([ "$rc" = 0 ] && echo ok)"
echo "3.90 3 3 1/1 1" >"$work/proc/loadavg"
gate
check "load just under 0.5 x CPUs passes" "$([ "$rc" = 0 ] && echo ok)"
echo "4.00 3 3 1/1 1" >"$work/proc/loadavg"
gate
check "load at 0.5 x CPUs is refused (strictly below)" "$([ "$rc" = 4 ] && echo ok)"
FAKE_NCPU=16 gate
check "the limit scales with the CPU count" "$([ "$rc" = 0 ] && echo ok)"
BENCH_QUIET_LOAD_FACTOR=0.1 FAKE_NCPU=16 gate
check "BENCH_QUIET_LOAD_FACTOR tightens it" "$([ "$rc" = 4 ] && echo ok)"

reset_host
cat >"$FAKE_PS" <<'EOF'
    1     0 init
   10     1 bash
   50    10 cargo
  100    50 bench.sh
  101   100 rustc
  102   101 cc1plus
  200     1 qemu-system-aarch64
  201     1 emulator64-crash-service
  202     1 emu
  203     1 simee
  204     1 launchd_sim
  205     1 clang
  206     1 cargo
  300    10 rustc
EOF
r=$(reasons)
check "a foreign emulator is named with its PID" "$(has 'process: emulator qemu-system-aarch64 (pid 200)' "$r" && has 'emulator emu (pid 202)' "$r" && has 'emulator64-crash-service (pid 201)' "$r" && echo ok)"
check "a foreign simulator and simee process are named" "$(has 'simulator simee (pid 203)' "$r" && has 'simulator launchd_sim (pid 204)' "$r" && echo ok)"
check "a foreign compiler is named" "$(has 'compiler clang (pid 205)' "$r" && has 'compiler cargo (pid 206)' "$r" && echo ok)"
check "an ancestor (the cargo that started this run) is not foreign" "$(hasnt '(pid 50)' "$r" && echo ok)"
check "this shell's own descendants are not foreign" "$(hasnt '(pid 101)' "$r" && hasnt '(pid 102)' "$r" && echo ok)"
check "a sibling of an ancestor is foreign" "$(has '(pid 300)' "$r" && echo ok)"
gate
check "foreign processes refuse the gate" "$([ "$rc" = 4 ] && has 'process: compiler cargo (pid 206)' "$err" && echo ok)"
r=$(BENCH_QUIET_ALLOW_PIDS="200 201 202 203 204 205 206 300" reasons)
check "BENCH_QUIET_ALLOW_PIDS names the run's own processes" "$([ -z "$r" ] && echo ok)"
cat >"$FAKE_PS" <<'EOF'
    1     0 init
  100     1 bench.sh
  400   100 emu
  401   400 qemu-system-aarch64
EOF
check "a device this run started (below this shell) is its own" "$([ -z "$(reasons)" ] && echo ok)"
cat >"$FAKE_PS" <<'EOF'
    1     0 init
  100     1 bench.sh
  500     1 cargo-watch
  501     1 simeectl
  502     1 emulator-notes
  503     1 vim
EOF
check "a name that only starts like one is not matched" "$([ -z "$(reasons)" ] && echo ok)"
cat >"$FAKE_PS" <<'EOF'
    1     0 init
  100     1 bench.sh
  600     1 g++
  601     1 clang++
  602     1 com.apple.CoreSimulator.CoreSimulatorService
  603     1 clangd
  604     1 comXapple.CoreSimulator.CoreSimulatorService
EOF
r=$(reasons)
check "names with regex characters match exactly (g++, clang++, a dotted service), nothing near them" "$(has 'compiler g++ (pid 600)' "$r" && has 'compiler clang++ (pid 601)' "$r" && has 'CoreSimulatorService (pid 602)' "$r" && hasnt '(pid 603)' "$r" && hasnt '(pid 604)' "$r" && echo ok)"

reset_host
zone cpu 91000
gate
check "a hot thermal zone refuses" "$([ "$rc" = 4 ] && has 'thermal: thermal_zone0 cpu 91 C' "$err" && echo ok)"
zone cpu 52000
mkdir -p "$work/sys/class/thermal/cooling_device0" "$work/sys/class/thermal/cooling_device1"
echo Processor >"$work/sys/class/thermal/cooling_device0/type"
echo 0 >"$work/sys/class/thermal/cooling_device0/cur_state"
echo 3 >"$work/sys/class/thermal/cooling_device0/max_state"
echo Fan >"$work/sys/class/thermal/cooling_device1/type"
echo 2 >"$work/sys/class/thermal/cooling_device1/cur_state"
gate
check "a cool zone, an idle processor and a spinning fan pass" "$([ "$rc" = 0 ] && echo ok)"
echo 2 >"$work/sys/class/thermal/cooling_device0/cur_state"
gate
check "a throttled processor cooling device refuses" "$([ "$rc" = 4 ] && has 'Processor throttled (2 of 3)' "$err" && echo ok)"
facts=$("$here/quiet.sh" facts)
check "facts report thermal pressure" "$(has thermal=pressure "$facts" && echo ok)"

reset_host
supply BAT0 Battery status Discharging present 1
supply AC Mains online 0
gate
check "a laptop on battery is refused" "$([ "$rc" = 4 ] && has 'power: on battery' "$err" && echo ok)"
supply BAT0 Battery status Charging present 1
supply AC Mains online 1
gate
check "a laptop on AC passes" "$([ "$rc" = 0 ] && echo ok)"
check "facts say it is a laptop on ac" "$(f=$("$here/quiet.sh" facts) && has power=ac "$f" && has laptop=1 "$f" && echo ok)"
reset_host
supply AC Mains online 1
check "a machine with only a mains supply is on ac, not a laptop" "$(f=$("$here/quiet.sh" facts) && has power=ac "$f" && has laptop=0 "$f" && echo ok)"
reset_host
check "a host with no sensors and no supplies passes and says so" "$(f=$("$here/quiet.sh" facts) && has thermal=unavailable "$f" && has power=unknown "$f" && echo ok)"

# --- macOS, through the stubs -----------------------------------------------------------------
mac() {
  reset_host
  export BENCH_QUIET_OS=Darwin
  echo "{ 1.20 1.10 1.00 }" >"$FAKE_LOADAVG"
  printf 'Note: No thermal warning level has been recorded\nNote: No performance warning level has been recorded\nNote: No CPU power status has been recorded\nCPU_Scheduler_Limit \t= 100\nCPU_Available_CPUs \t= 8\nCPU_Speed_Limit \t= 100\n' >"$FAKE_THERM"
  printf "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%%; charged; 0:00 remaining present: true\n" >"$FAKE_BATT"
  echo "Current pressure level: Nominal" >"$FAKE_PM"
  cat >"$FAKE_PS" <<'EOF'
    1     0 /sbin/launchd
   50     1 /bin/zsh
  100    50 /bin/bash
EOF
}
mac
cat >>"$FAKE_PS" <<'EOF'
  700     1 /Applications/Xcode.app/Contents/Developer/Applications/Simulator.app/Contents/MacOS/Simulator
EOF
r=$(reasons)
check "macOS: a Simulator.app process is foreign, named by the basename of its path" "$(has 'simulator Simulator (pid 700)' "$r" && echo ok)"
mac
cat >>"$FAKE_PS" <<'EOF'
  710     1 /Applications/Android Studio.app/Contents/emulator/emulator
EOF
r=$(reasons)
check "macOS: a path with a space is read to its basename" "$(has 'emulator emulator (pid 710)' "$r" && echo ok)"
mac
gate
check "macOS: a quiet host passes (load from sysctl, 0.5 x 8)" "$([ "$rc" = 0 ] && echo ok)"
echo "{ 9.50 8.00 7.00 }" >"$FAKE_LOADAVG"
gate
check "macOS: a loaded host is refused" "$([ "$rc" = 4 ] && has 'is 9.50, not below 4.00' "$err" && echo ok)"
mac
printf 'CPU_Scheduler_Limit \t= 75\nCPU_Available_CPUs \t= 8\nCPU_Speed_Limit \t= 70\n' >"$FAKE_THERM"
gate
check "macOS: pmset's CPU_Speed_Limit under 100 is thermal pressure" "$([ "$rc" = 4 ] && has 'CPU_Speed_Limit=70' "$err" && echo ok)"
mac
printf "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t80%%; discharging; 3:00 remaining present: true\n" >"$FAKE_BATT"
gate
check "macOS: battery power is refused" "$([ "$rc" = 4 ] && has 'power: on battery' "$err" && echo ok)"
mac
FAKE_UID=0
export FAKE_UID
echo "Current pressure level: Heavy" >"$FAKE_PM"
gate
check "macOS: powermetrics (as root) reporting Heavy is thermal pressure" "$([ "$rc" = 4 ] && has 'powermetrics: Heavy' "$err" && echo ok)"
echo "Current pressure level: Nominal" >"$FAKE_PM"
gate
check "macOS: powermetrics Nominal passes" "$([ "$rc" = 0 ] && echo ok)"
FAKE_UID=1000
f=$("$here/quiet.sh" facts)
check "macOS: without root and without passwordless sudo it says powermetrics wasn't read" "$(has 'powermetrics: not read' "$f" && has thermal=nominal "$f" && echo ok)"
check "macOS: facts carry power and laptop" "$(has power=ac "$f" && has laptop=1 "$f" && echo ok)"

# --- the bounded wait --------------------------------------------------------------------------
reset_host
echo "13.00 1 1 1/1 1" >"$work/proc/loadavg"
(
  sleep 1
  echo "0.10 1 1 1/1 1" >"$work/proc/loadavg"
) &
out=$(BENCH_QUIET_WAIT=10 "$here/quiet.sh" gate 2>/dev/null)
rc=$?
wait
check "a host that quietens within the wait passes, and says it waited" "$([ "$rc" = 0 ] && has 'passed after' "$out" && echo ok)"
echo "13.00 1 1 1/1 1" >"$work/proc/loadavg"
t0=$(date +%s)
BENCH_QUIET_WAIT=2 gate
t1=$(date +%s)
check "a host that stays busy is refused after the wait, not before or long after" "$([ "$rc" = 4 ] && [ $((t1 - t0)) -ge 2 ] && [ $((t1 - t0)) -le 5 ] && echo ok)"
check "the refusal says how long it looked" "$(has 'looked for 2 s' "$err" && echo ok)"

# --- the record and the spread -----------------------------------------------------------------
reset_host
py() { python3 -I -c '
import importlib.util, sys
sys.dont_write_bytecode = True  # no __pycache__ beside the script (-I ignores PYTHONDONTWRITEBYTECODE)
spec = importlib.util.spec_from_file_location("summarize", sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
'"$1" "$here/summarize.py" "${@:2}"; }
check "percentiles interpolate: p50 of 1..5 is 3, p95 is 4.8, IQR is 2" "$(py 'v=[5,1,3,2,4]; assert m.percentile(v,50)==3 and abs(m.percentile(v,95)-4.8)<1e-9 and m.iqr(v)==2; print("ok")')"
check "spread reports p50, p95, IQR, stddev and the relative IQR" "$(py 's=m.spread([10,10.2,11],0.1); assert s["p50"]==10.2 and s["n"]==3 and abs(s["iqr"]-0.5)<1e-9 and not s["noisy"] and s["stddev"]>0; print("ok")')"
check "a spread over its budget is noisy, at 3 samples" "$(py 's=m.spread([10,14,20],0.1); assert s["noisy"]; print("ok")')"
check "two samples are not judged" "$(py 's=m.spread([10,20],0.1); assert not s["noisy"]; print("ok")')"
check "a median of 0 with a spread is noisy, with none it is quiet" "$(py 'assert m.spread([0,0,5],0.1)["noisy"] and not m.spread([0,0,0],0.1)["noisy"]; print("ok")')"
check "budgets: seconds 0.10, p99 0.30, CPU 0.25, failures none, the default 0.15" "$(py 'b=m.noise_budget; assert (b("boot, s"),b("frame time p99, ms"),b("idle host CPU, % of a core"),b("failures"),b("whatever"))==(0.10,0.30,0.25,None,0.15), (b("boot, s"),b("frame time p99, ms")); print("ok")')"
check "BENCH_NOISE_BUDGET replaces every budget" "$(BENCH_NOISE_BUDGET=0.5 py 'assert m.noise_budget("boot, s")==0.5 and m.noise_budget("failures")==0.5; print("ok")')"

# A benchmark of its own, through lib.sh: bench_start's gate, bench_sample, bench_finish.
cat >"$work/fakebench.sh" <<'EOF'
#!/usr/bin/env bash
# Fake: samples FAKE_VALUES (space-separated) as "boot, s" for arm a, one per round.
set -uo pipefail
. "$LIB"
while [ $# -gt 0 ]; do
  bench_opt "$@"
  shift "$BENCH_SHIFT"
done
bench_start fake
for v in $FAKE_VALUES; do
  BENCH_ROUND=$((BENCH_ROUND + 1))
  bench_sample a "boot, s" "$v"
done
bench_finish "" "fake"
EOF
chmod +x "$work/fakebench.sh"
export LIB="$here/lib.sh"
fake() { # args...; sets out, err, rc, and the newest host record in $rec
  rm -rf "$work/res"
  out=$(BENCH_OUT="$work/res" "$work/fakebench.sh" "$@" 2>"$work/err")
  rc=$?
  err=$(cat "$work/err")
  rec=$(find "$work/res" -name '*.host.json' 2>/dev/null | head -1)
}
fields() { python3 -I -c '
import json, sys
r = json.load(open(sys.argv[1]))
need = "bench bench_version rounds model chip cores cores_physical mem_gb os os_version load1_start load1_end thermal_start thermal_end power_start power_end commit date quiet_gate publishable not_publishable".split()
missing = [k for k in need if k not in r]
assert not missing, missing
assert r["bench"] == "fake" and r["bench_version"].startswith("fakebench.sh@") and r["rounds"] == int(sys.argv[2])
print("ok")' "$1" "$2"; }

export FAKE_VALUES="10 10.2 10.4"
fake
check "a bench on a quiet host runs and writes its result and host record" "$([ "$rc" = 0 ] && [ -n "$rec" ] && [ -f "${rec%.host.json}.json" ] && echo ok)"
check "the host record is JSON with the issue's fields" "$([ -n "$rec" ] && fields "$rec" 3 2>&1)"
check "it records the gate and calls a stable, gated, 3-round run publishable" "$(python3 -I -c '
import json, sys
r = json.load(open(sys.argv[1]))
assert r["quiet_gate"] == "passed" and r["publishable"] is True and r["not_publishable"] == [], r
print("ok")' "$rec")"
check "the same record is inside the result JSON, with the spread" "$(python3 -I -c '
import json, sys
r = json.load(open(sys.argv[1]))
s = r["spread"]["a"]["boot, s"]
assert r["record"]["bench"] == "fake" and s["p50"] == 10.2 and s["noisy"] is False and "p95" in s and "iqr" in s
print("ok")' "${rec%.host.json}.json")"
check "the printed table carries the gate line" "$(has 'Quiet gate: passed' "$out" && hasnt NOISY "$out" && echo ok)"
check "summarize.py check passes it" "$(python3 -I "$here/summarize.py" check "$rec" >/dev/null && echo ok)"

FAKE_VALUES="10 14 20" fake
check "a noisy run is marked NOISY in the table" "$([ "$rc" = 0 ] && has 'NOISY, spread over budget' "$out" && has 'NOT PUBLISHABLE' "$out" && echo ok)"
check "and its record says it is not publishable" "$(python3 -I -c '
import json, sys
r = json.load(open(sys.argv[1]))
assert r["publishable"] is False and r["noisy"] and "noisy" in r["not_publishable"][0], r
print("ok")' "$rec")"
check "summarize.py check fails it" "$(! python3 -I "$here/summarize.py" check "$rec" >/dev/null && echo ok)"

fake --quick
check "a --quick run is not publishable" "$(has 'smoke test' "$(cat "$rec")" && has '"publishable": false' "$(cat "$rec")" && echo ok)"

echo "13.00 1 1 1/1 1" >"$work/proc/loadavg"
fake
check "a loaded host: the bench exits 4 and measures nothing" "$([ "$rc" = 4 ] && [ -z "$rec" ] && [ -z "$out" ] && echo ok)"
check "the refusal reaches the user" "$(has 'refused: this host is not quiet for fakebench.sh' "$err" && has 'the 1-minute load is 13.00' "$err" && echo ok)"
fake --no-quiet-gate
check "--no-quiet-gate runs anyway" "$([ "$rc" = 0 ] && [ -n "$rec" ] && echo ok)"
check "and its record says the gate was skipped and is not publishable" "$(has 'disabled (--no-quiet-gate)' "$(cat "$rec")" && has '"publishable": false' "$(cat "$rec")" && echo ok)"
help=$("$work/fakebench.sh" --help 2>&1)
check "--help mentions the gate" "$(has -- '--no-quiet-gate' "$help" && echo ok)"

# --- run-all.sh --------------------------------------------------------------------------------
reset_host
mkdir -p "$work/ra"
# shellcheck disable=SC2016 # the child's $0 and $* expand when it runs
printf '#!/usr/bin/env bash\necho "$0 $*" >> "%s/ra/ran"\nexit 0\n' "$work" >"$work/ra/ok.sh"
# shellcheck disable=SC2016
printf '#!/usr/bin/env bash\necho "$0 $*" >> "%s/ra/ran"\nexit 4\n' "$work" >"$work/ra/refused.sh"
# shellcheck disable=SC2016
printf '#!/usr/bin/env bash\necho "$0 $*" >> "%s/ra/ran"\nexit 0\n' "$work" >"$work/ra/after.sh"
chmod +x "$work"/ra/*.sh
printf 'ok|host|ok.sh|x\nref|host|refused.sh|x\nafter|host|after.sh|x\n' >"$work/ra/registry"
runall() { # args...; sets out, rc
  : >"$work/ra/ran"
  out=$(RUN_ALL_REGISTRY="$work/ra/registry" RUN_ALL_DIR="$work/ra" "$here/run-all.sh" "$@" 2>&1 </dev/null)
  rc=$?
}
runall
check "run-all stops at a refusing benchmark and exits 4" "$([ "$rc" = 4 ] && has 'REFUSED  ref' "$out" && ! grep -q after.sh "$work/ra/ran" && echo ok)"
runall --no-quiet-gate --only ok
check "run-all passes --no-quiet-gate to each benchmark" "$([ "$rc" = 0 ] && grep -q -- '--no-quiet-gate' "$work/ra/ran" && echo ok)"
runall --quiet --quick
check "run-all --quiet refuses --quick" "$([ "$rc" = 2 ] && has 'smoke test' "$out" && echo ok)"
runall --quiet --rounds 2
check "run-all --quiet refuses fewer than 3 rounds" "$([ "$rc" = 2 ] && has '3 or more rounds' "$out" && echo ok)"
runall --quiet --no-quiet-gate
check "run-all --quiet refuses --no-quiet-gate" "$([ "$rc" = 2 ] && has contradict "$out" && echo ok)"
echo "13.00 1 1 1/1 1" >"$work/proc/loadavg"
runall --quiet
check "run-all --quiet looks first: a loaded host runs nothing" "$([ "$rc" = 4 ] && [ ! -s "$work/ra/ran" ] && has 'refused: this host is not quiet' "$out" && echo ok)"
echo "0.10 1 1 1/1 1" >"$work/proc/loadavg"
# A benchmark that writes a record: a noisy one is reported as NOISY and fails the run.
mkdir -p "$work/ra/out"
printf '#!/usr/bin/env bash\nBENCH_OUT=%s FAKE_VALUES="10 14 20" exec %s/fakebench.sh --out %s "$@"\n' "$work/ra/out" "$work" "$work/ra/out" >"$work/ra/noisy.sh"
printf '#!/usr/bin/env bash\nFAKE_VALUES="10 10.1 10.2" exec %s/fakebench.sh "$@"\n' "$work" >"$work/ra/steady.sh"
chmod +x "$work"/ra/*.sh
printf 'steady|host|steady.sh|x\n' >"$work/ra/registry"
out=$(RUN_ALL_REGISTRY="$work/ra/registry" RUN_ALL_DIR="$work/ra" "$here/run-all.sh" --quiet --out "$work/ra/out" 2>&1 </dev/null)
rc=$?
check "run-all --quiet reports a steady benchmark as ok" "$([ "$rc" = 0 ] && has 'ok       steady' "$out" && echo ok)"
printf 'noisy|host|noisy.sh|x\n' >"$work/ra/registry"
out=$(RUN_ALL_REGISTRY="$work/ra/registry" RUN_ALL_DIR="$work/ra" "$here/run-all.sh" --quiet --out "$work/ra/out" 2>&1 </dev/null)
rc=$?
check "run-all --quiet reports a noisy benchmark as NOISY and exits 1" "$([ "$rc" = 1 ] && has 'NOISY    noisy' "$out" && echo ok)"

[ "$fails" = 0 ] || {
  echo "$fails failed"
  exit 1
}
echo "all passed"
