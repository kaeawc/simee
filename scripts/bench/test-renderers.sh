#!/usr/bin/env bash
# Tests for renderers.sh's device and process safety, with no device and no emulator:
#   - a protected console or adb port is refused whether or not anything listens on it, and the
#     default port is not a protected one;
#   - a process is signalled only while its PID still has the identity recorded at launch (a
#     reused PID is left alone: lib.sh's identity is tested by test-lib.sh), and the console quit
#     goes out only to the process that listens on the port;
#   - the ports are checked right before a launch, after the wait for a boot slot;
#   - the boot gate's waiters are stopped at exit only while they are still the ones started;
#   - a failed run's samples are dropped and counted.
#
#   scripts/bench/test-renderers.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/test-renderers.XXXXXX")
pids=()
cleanup() {
  local p
  for p in ${pids[@]+"${pids[@]}"}; do kill "$p" 2>/dev/null; done
  rm -rf "$work"
}
trap cleanup EXIT
fails=0
check() { # description, condition result
  if [ "$2" = ok ]; then echo "ok   $1"; else
    echo "FAIL $1"
    fails=$((fails + 1))
  fi
}
skip() { echo "skip $1"; }
yes_if() { if "$@"; then echo ok; fi; }
no_if() { if ! "$@"; then echo ok; fi; }

# --- The script's own command line (dry run: nothing is built or booted) ---

plan() { # args; sets out and rc
  out=$("$here/renderers.sh" --dry-run --arms lavapipe "$@" 2>&1)
  rc=$?
}
refused() { [ "$rc" = 2 ] && printf %s "$out" | grep -q "$1" && echo ok; }
for port in 5554 5556 5558 5590 5680; do
  plan --port "$port"
  check "--port $port is refused (status 2, protected)" "$(refused 'protected device')"
done
for port in 5553 5555 5557; do
  plan --port "$port"
  check "--port $port is refused: odd, and its adb port is a protected console's" "$([ "$rc" = 2 ] && echo ok)"
done
BENCH_PROTECTED_DEVICES="emulator-5701" plan
check "a port whose adb serial is protected is refused" "$(refused 'emulator-5701 is a protected')"
plan --port 05554
check "--port 05554 (leading zero) is refused" "$(refused 'protected device')"
plan --port 5701
check "an odd port is refused" "$(refused odd)"
plan --port abc
check "a port that is not a number is refused" "$(refused 'not a number')"
plan --port 99999999999999999999
check "a huge port is refused" "$(refused 'out of range')"
BENCH_PROTECTED_DEVICES="emulator-5700" plan
check "BENCH_PROTECTED_DEVICES adds to the list, default port included" "$(refused 'protected device')"
plan
check "the default port is 5700, outside the protected list" "$([ "$rc" = 0 ] && printf %s "$out" | grep -q 'port 5700 (adb 5701)' && echo ok)"
plan --port 5650
check "an unprotected even port is accepted" "$([ "$rc" = 0 ] && printf %s "$out" | grep -q 'port 5650 (adb 5651)' && echo ok)"
plan --cold-only --port 5554
check "--cold-only uses no device, so its port isn't checked" "$([ "$rc" = 0 ] && echo ok)"

# --- Process identity and the guarded stop (functions only) ---

# shellcheck disable=SC1091 # the script under test
RENDERERS_SOURCE_ONLY=1 . "$here/renderers.sh"
EMU_QUIT_WAIT=1  # the fake adb quits nothing
EMU_STOP_GRACE=1 # background jobs ignore SIGINT here, as the stand-ins below do: SIGTERM after 1 s
BENCH_WORK=$work
BENCH_SAMPLES="$work/samples.tsv"
: >"$BENCH_SAMPLES"
fake_log="$work/adb.log"
BENCH_ADB="$work/adb"
printf '#!/bin/sh\necho "$@" >> "%s"\n' "$fake_log" >"$BENCH_ADB"
chmod +x "$BENCH_ADB"
bench_log() { :; }

sleep 60 &
live=$!
pids+=("$live")
ident="$(bench_pid_start "$live")|"
check "a live process has an identity" "$([ "$ident" != "|" ] && echo ok)"

# A stale record: the PID is alive but isn't the process that was started.
: >"$fake_log"
emu_stop "$live" "Thu Jan  1 00:00:00 1970|" 5700
check "emu_stop leaves a PID whose identity differs alone" "$(yes_if kill -0 "$live")"
check "emu_stop sends no console quit for it" "$([ ! -s "$fake_log" ] && echo ok)"
emu_stop "$live" "" 5700
check "emu_stop with no identity recorded signals nothing" "$(yes_if kill -0 "$live")"
emu_stop "" "" 5700
check "emu_stop with no PID is a no-op" "$([ ! -s "$fake_log" ] && echo ok)"

# The real one, not listening on the port: SIGINT, and no console quit.
emu_stop "$live" "$ident" 5700
check "emu_stop stops the process it started" "$(no_if kill -0 "$live" 2>/dev/null)"
check "emu_stop sends no console quit when the process doesn't own the port" "$([ ! -s "$fake_log" ] && echo ok)"

if command -v lsof >/dev/null 2>&1; then
  lport=$((20000 + $$ % 20000))
  lport=$((lport - lport % 2))
  python3 -I -c '
import socket, sys, time
s = socket.socket()
s.bind(("127.0.0.1", int(sys.argv[1])))
s.listen()
time.sleep(60)' "$lport" &
  owner=$!
  pids+=("$owner")
  for _ in $(seq 1 50); do
    owns_port "$owner" "$lport" && break
    sleep 0.1
  done
  check "owns_port: the listener owns its port" "$(yes_if owns_port "$owner" "$lport")"
  check "owns_port: another PID doesn't" "$(no_if owns_port "$$" "$lport")"
  check "owns_port: nobody owns a closed port" "$(no_if owns_port "$owner" $((lport + 1)))"
  sleep 60 &
  other=$!
  pids+=("$other")
  other_ident="$(bench_pid_start "$other")|"
  : >"$fake_log"
  emu_stop "$other" "$other_ident" "$lport"
  check "the port's owner is not this process: no console quit reaches it" "$([ ! -s "$fake_log" ] && echo ok)"
  check "the process this run started still stops" "$(no_if kill -0 "$other" 2>/dev/null)"
  emu_stop "$owner" "$(bench_pid_start "$owner")|" "$lport"
  check "the process on the port gets the console quit for its own serial" "$(grep -qx -- "-s emulator-$lport emu kill" "$fake_log" && echo ok)"
  check "and is stopped" "$(no_if kill -0 "$owner" 2>/dev/null)"
else
  skip "owns_port and the console quit (no lsof)"
fi

# --- The boot gate ---

GATE_GRACE=1
BENCH_PIDS=()
sleep 60 &
stale=$!
pids+=("$stale")
GATE_SLOTS=("$stale|Thu Jan  1 00:00:00 1970")
GATE_DONE=""
gate_cleanup
check "gate_cleanup leaves a PID that is no longer its waiter alone" "$(yes_if kill -0 "$stale")"
check "gate_cleanup empties its list" "$([ "${#GATE_SLOTS[@]}" = 0 ] && echo ok)"
sleep 60 &
waiter=$!
pids+=("$waiter")
GATE_SLOTS=("$waiter|$(bench_pid_start "$waiter")")
gate_cleanup
check "gate_cleanup stops a waiter still running that it started" "$(no_if kill -0 "$waiter" 2>/dev/null)"
sleep 60 &
gone=$!
gone_entry="$gone|$(bench_pid_start "$gone")"
kill "$gone"
wait "$gone" 2>/dev/null
GATE_SLOTS=("$gone_entry" "$live|x")
gate_reap
check "gate_reap drops waiters that finished or aren't the ones started" "$([ "${#GATE_SLOTS[@]}" = 0 ] && echo ok)"

# A gate that exits without taking a slot: acquire fails and nothing is left to signal later.
printf '#!/bin/sh\nexit 1\n' >"$work/badgate"
chmod +x "$work/badgate"
gate="$work/badgate"
# shellcheck disable=SC2218 # the real one, from renderers.sh; the boot tests below stub it
gate_acquire pretend
check "gate_acquire fails when the gate exits without a slot" "$([ $? = 1 ] && echo ok)"
check "and leaves no waiter to signal at exit" "$([ "${#GATE_SLOTS[@]}" = 0 ] && echo ok)"

# --- The arms' environments ---

lib64=/sdk/lib64
env_has() { case ",$(arm_env "$1")," in *",$2,"*) echo ok ;; *) ;; esac }
for arm in mvk-metal mvk-swangle; do
  check "$arm names the MoltenVK ICD explicitly" "$(env_has "$arm" ANDROID_EMU_VK_ICD=moltenvk)"
  check "$arm doesn't point the loader at another manifest" "$([ -z "$(env_has "$arm" "VK_DRIVER_FILES=$lib64/vulkan/libkosmickrisp_icd.json")" ] && echo ok)"
done
for arm in kk-metal kk-swangle; do
  check "$arm names the KosmicKrisp ICD explicitly" "$(env_has "$arm" ANDROID_EMU_VK_ICD=kosmickrisp)"
done
check "mvk-metal keeps ANGLE on Metal" "$(env_has mvk-metal ANGLE_DEFAULT_PLATFORM=metal)"
check "mvk-swangle keeps SwANGLE" "$(env_has mvk-swangle ANGLE_DEFAULT_PLATFORM=swiftshader)"
check "no arm names two ICDs" "$(for arm in mvk-metal mvk-swangle kk-metal kk-swangle; do arm_env "$arm" | tr ',' '\n' | grep -c '^ANDROID_EMU_VK_ICD='; done | sort -u | tr -d '\n' | grep -qx 1 && echo ok)"
check "the MoltenVK arms keep emu's renderer-only init on the same ICD" "$(case $(init_env mvk-metal) in ANDROID_EMU_VK_ICD=moltenvk,*) echo ok ;; *) ;; esac)"
lib64=""

# --- A boot: the ports are checked after the slot wait, right before the launch ---

# boot_arm with its collaborators replaced by ones that record the order they are called in.
order="$work/order.log"
arm_env() { echo ""; }
arm_gpu() { echo host; }
load1() { echo 0; }
now() { echo 0; }
gate_release() { echo release >>"$order"; }
port_clear() { echo port_clear >>"$order"; }
android_boot() {
  echo android_boot >>"$order"
  BOOT_PID=4242
  BOOT_IDENT="Thu Jan  1 00:00:00 1970|/bin/emu"
  return "$stub_boot_rc"
}
gate_acquire() {
  echo gate_acquire >>"$order"
  return "$stub_gate_rc"
}
AVD=avd
port=5700
: >"$work/boot.log"
stub_gate_rc=0
stub_boot_rc=0
: >"$order"
boot_arm lavapipe "$work/boot.log"
check "boot_arm: the slot first, then the ports, then the launch, then the slot is released" "$([ "$(tr '\n' ' ' <"$order")" = "gate_acquire port_clear android_boot release " ] && echo ok)"
check "boot_arm: the identity is the one recorded at launch" "$([ "$BOOT_PID" = 4242 ] && [ "$BOOT_IDENT" = "Thu Jan  1 00:00:00 1970|/bin/emu" ] && echo ok)"
stub_gate_rc=1
BOOT_IDENT=old
: >"$order"
boot_arm lavapipe "$work/boot.log"
check "boot_arm: without a slot it returns 2" "$([ $? = 2 ] && echo ok)"
check "boot_arm: without a slot the ports aren't checked and nothing launches" "$([ "$(tr '\n' ' ' <"$order")" = "gate_acquire " ] && echo ok)"
check "boot_arm: a previous boot's identity doesn't carry over" "$([ -z "$BOOT_IDENT" ] && [ -z "$BOOT_PID" ] && echo ok)"

# --- Failed runs ---

BENCH_ROUND=1
buf_begin
bench_sample "arm A" "idle" 0
buf_end drop
check "a dropped run leaves no sample" "$([ ! -s "$BENCH_SAMPLES" ] && echo ok)"
buf_begin
bench_sample "arm A" "idle" 7
buf_end keep
check "a kept run's samples join the others" "$([ "$(wc -l <"$BENCH_SAMPLES")" = 1 ] && grep -q 'idle	7' "$BENCH_SAMPLES" && echo ok)"
check "no failures: the note says none" "$([ "$(failure_note)" = "Failed runs: none." ] && echo ok)"
: >"$BENCH_SAMPLES"
run_failed "arm A" warm crashed
run_failed "arm A" warm crashed
run_failed "arm B" first never
check "a failure is a word in the boot-time row, which the medians skip" "$([ "$(wc -l <"$BENCH_SAMPLES")" = 3 ] && grep -q 'warm boot: launch until sys.boot_completed, s	crashed' "$BENCH_SAMPLES" && echo ok)"
check "the note counts failures per arm and kind" "$([ "$(failure_note)" = "Failed runs, left out of the medians: arm A warm boot crashed (2); arm B first boot never (1)." ] && echo ok)"
python3 -I -B - "$here/summarize.py" "$BENCH_SAMPLES" <<'EOF' >"$work/median.out"
import importlib.util, sys
spec = importlib.util.spec_from_file_location("summarize", sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
samples = m.read_samples(sys.argv[2])
samples.append({"round": 3, "arm": "arm A", "metric": "warm boot: launch until sys.boot_completed, s", "value": 17.0, "load1": None})
samples.append({"round": 4, "arm": "arm A", "metric": "warm boot: launch until sys.boot_completed, s", "value": 18.0, "load1": None})
import json, os, tempfile
out = os.path.join(tempfile.mkdtemp(), "o.json")
m.finish("t", samples, {}, {}, out)
print(json.load(open(out))["medians"]["arm A"]["warm boot: launch until sys.boot_completed, s"])
EOF
check "the medians skip a failed run's word (17 and 18, not crashed)" "$([ "$(cat "$work/median.out")" = 17.5 ] && echo ok)"

echo
if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; fi
exit "$([ "$fails" = 0 ] && echo 0 || echo 1)"
