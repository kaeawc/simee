#!/usr/bin/env bash
# Tests for lib.sh's process identity, with no device and no emulator: a tracked PID is signalled
# only while it is still the process that was started (its start time and, where one was given,
# its command), so a PID macOS hands to another process is never signalled, and the scripts that
# track no identity behave as before.
#
#   scripts/bench/test-lib.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/test-lib.XXXXXX")
pids=()
# shellcheck disable=SC2329 # run by the EXIT trap below
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
yes_if() { if "$@"; then echo ok; fi; }
no_if() { if ! "$@"; then echo ok; fi; }
# shellcheck disable=SC2329 # named as the command of yes_if and no_if
alive() { kill -0 "$1" 2>/dev/null; }
# shellcheck disable=SC1091 # the script under test
. "$here/lib.sh"
# shellcheck disable=SC2329 # lib.sh's calls it: silenced here
bench_log() { :; }
BENCH_KEEP_WORK=1
STALE="Thu Jan  1 00:00:00 1970"
sleep_bin=$(command -v sleep)
spawn() { # a stand-in that runs "sleep 60" by its full path, so its command is known; sets $spawned
  local i
  "$sleep_bin" 60 &
  spawned=$!
  pids+=("$spawned")
  # Until the exec, a signal can reach the forked shell before it acts on it: wait for the exec.
  for i in $(seq 1 50); do
    case $(bench_pid_cmd "$spawned") in "$sleep_bin"*) break ;; *) sleep 0.1 ;; esac
  done
}

# --- The identity of a process ---

spawn
live=$spawned
started=$(bench_pid_start "$live")
check "a live process has a start time" "$([ -n "$started" ] && echo ok)"
check "a PID with no process has none" "$([ -z "$(bench_pid_start 2147483000)" ] && echo ok)"
check "bench_pid_is: the start time alone" "$(yes_if bench_pid_is "$live" "$started|")"
check "bench_pid_is: the start time and its command" "$(yes_if bench_pid_is "$live" "$started|$sleep_bin 60")"
check "bench_pid_is: not when the start time differs (a reused PID)" "$(no_if bench_pid_is "$live" "$STALE|")"
check "bench_pid_is: not when the command differs" "$(no_if bench_pid_is "$live" "$started|/bin/other 60")"
check "bench_pid_is: not with no start time recorded" "$(no_if bench_pid_is "$live" "|")"
check "bench_pid_is: not with no identity" "$(no_if bench_pid_is "$live" "")"
check "bench_pid_is: not with no PID" "$(no_if bench_pid_is "" "$started|")"

# --- Tracking records the identity at launch ---

BENCH_PIDS=()
bench_track "$live"
check "bench_track records PID|start|, with no command when none is given" "$([ "${BENCH_PIDS[0]}" = "$live|$started|" ] && echo ok)"
check "bench_identity returns what it recorded" "$([ "$(bench_identity "$live")" = "$started|" ] && echo ok)"
check "bench_identity fails for a PID that isn't tracked" "$(no_if bench_identity 1)"
BENCH_PIDS=()
BENCH_TRACK_WAIT=10 # 1 s
bench_track "$live" "$sleep_bin"
check "bench_track with a command records the command line that starts with it" "$([ "$(bench_identity "$live")" = "$started|$sleep_bin 60" ] && echo ok)"
BENCH_PIDS=()
# A launch is a fork, then an exec: the command is the launching shell's until then.
# shellcheck disable=SC2016 # the inner shell's $0 expands when it runs
"$BASH" -c 'sleep 0.5; exec "$0" 60' "$sleep_bin" &
late=$!
pids+=("$late")
BENCH_TRACK_WAIT=30 # 3 s
bench_track "$late" "$sleep_bin"
check "bench_track waits for the exec: it records the command, not the launcher's" "$([ "$(bench_identity "$late")" = "$(bench_pid_start "$late")|$sleep_bin 60" ] && echo ok)"
BENCH_PIDS=()
spawn
gone=$spawned
kill "$gone"
wait "$gone" 2>/dev/null
bench_track "$gone" "$sleep_bin"
check "a process already gone is recorded with no start time" "$([ "${BENCH_PIDS[0]}" = "$gone||" ] && echo ok)"
check "bench_untrack drops by PID and keeps the others" "$(
  BENCH_PIDS=("1|a|" "$gone||" "2|b|c")
  bench_untrack "$gone"
  [ "${BENCH_PIDS[*]}" = "1|a| 2|b|c" ] && echo ok
)"
BENCH_PIDS=()

# --- Stopping: a reused PID is never signalled ---

bench_stop_pid "$live" 1
check "bench_stop_pid: an untracked PID is signalled as before" "$(no_if alive "$live")"

spawn
reused=$spawned
BENCH_PIDS=("$reused|$STALE|")
bench_stop_pid "$reused" 1
check "bench_stop_pid: a tracked PID whose start time differs is left alone" "$(yes_if alive "$reused")"
check "and is dropped from the list" "$([ "${#BENCH_PIDS[@]}" = 0 ] && echo ok)"
BENCH_PIDS=("$reused|$(bench_pid_start "$reused")|/bin/other 60")
bench_stop_pid "$reused" 1
check "bench_stop_pid: a tracked PID whose command differs is left alone" "$(yes_if alive "$reused")"
BENCH_PIDS=("$reused||")
bench_stop_pid "$reused" 1
check "bench_stop_pid: a PID tracked with no start time is left alone" "$(yes_if alive "$reused")"
bench_stop_pid "$reused" 1 "$STALE|"
check "bench_stop_pid: an identity passed in that doesn't match leaves it alone" "$(yes_if alive "$reused")"
bench_stop_pid "$reused" 1 "$(bench_pid_start "$reused")|"
check "bench_stop_pid: one that matches stops it" "$(no_if alive "$reused")"

spawn
mine=$spawned
BENCH_PIDS=()
bench_track "$mine" "$sleep_bin"
bench_stop_pid "$mine" 1
check "bench_stop_pid stops a tracked process that is still the one started" "$(no_if alive "$mine")"
check "and untracks it" "$([ "${#BENCH_PIDS[@]}" = 0 ] && echo ok)"

# --- The exit cleanup ---

spawn
reused=$spawned
BENCH_PIDS=("$reused|$STALE|")
BENCH_CLEANUP=()
bench_on_exit "echo ran >'$work/cleanup.ran'"
(bench_cleanup) >/dev/null 2>&1
check "the exit cleanup leaves a tracked PID that was reused alone" "$(yes_if alive "$reused")"
check "and still runs the registered cleanups" "$([ -f "$work/cleanup.ran" ] && echo ok)"
kill "$reused"

# --- android_stop ---

fake_log="$work/adb.log"
BENCH_ADB="$work/adb"
# The fake adb's `emu kill` ends the emulator the way the real one does: the process exits.
# shellcheck disable=SC2016 # the fake's $@ and $(cat ...) expand when it runs
printf '#!/bin/sh\necho "$@" >> "%s"\n[ -f "%s" ] && kill "$(cat "%s")"\nexit 0\n' \
  "$fake_log" "$work/target.pid" "$work/target.pid" >"$BENCH_ADB"
chmod +x "$BENCH_ADB"
spawn
reused=$spawned
echo "$reused" >"$work/target.pid"
: >"$fake_log"
BENCH_PIDS=("$reused|$STALE|")
android_stop "$reused" 5700
check "android_stop: a tracked PID that was reused gets no console kill" "$([ ! -s "$fake_log" ] && echo ok)"
check "and no signal" "$(yes_if alive "$reused")"
check "and is untracked" "$([ "${#BENCH_PIDS[@]}" = 0 ] && echo ok)"
BENCH_PIDS=()
android_stop "$reused" 5700
check "android_stop: a PID that was never tracked gets the console kill for its own serial, as before" "$(grep -qx -- '-s emulator-5700 emu kill' "$fake_log" && echo ok)"
check "and the emulator it ended is gone" "$(no_if alive "$reused")"
spawn
mine=$spawned
echo "$mine" >"$work/target.pid"
: >"$fake_log"
BENCH_PIDS=()
bench_track "$mine" "$sleep_bin"
android_stop "$mine" 5700
check "android_stop: the process that was started gets the console kill" "$(grep -qx -- '-s emulator-5700 emu kill' "$fake_log" && echo ok)"
check "and is gone" "$(no_if alive "$mine")"

if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; fi
exit "$([ "$fails" = 0 ] && echo 0 || echo 1)"
