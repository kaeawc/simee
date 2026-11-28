#!/usr/bin/env bash
# iOS launchd jobs, one at a time: what each candidate for the slim profile costs a clone's settle
# time, its CPU once settled and its memory, against the same clone stock (#199).
#
#   scripts/bench/ios-slim.sh [--candidates "NAME ..."] [--idle S] [--settle-cpu foreground|all]
#                             [--device-type TYPE] [--runtime ID] [--simee BIN]
#                             [--rounds N] [--quick] [--out DIR] [--no-quiet-gate]
#
# Makes a device set of its own (a new directory; every simee call names it with --set, so no
# Simulator.app window attaches and no other device is seen), creates a template in it, boots the
# template once until settled and shuts it down, clones it, and boots the clone once until settled
# (its first boot, not measured). Each candidate is resolved against the clone's `launchctl list`:
# a full label as given, a bare name (`GSSCred`) as the one label whose last part it is; one that
# isn't a job in this runtime, or names several, is reported and not measured. Then each round,
# for the stock arm and for each candidate alone, in turn (order reversed on even rounds): the
# candidate is disabled in the clone (`launchctl disable system/<label>` inside it), the clone is
# shut down and booted with `simee boot --settle` (no profile), and the run records the settle
# time and CPU-s to settle (simee's own), the CPU of the clone's launchd_sim tree over the --idle
# seconds after it settled (each process's increase, a new one counting all of its time), its
# footprint and process count then, and whether the disable held (listed disabled, not running).
# The label is enabled again before the next arm, so every arm is a warm reboot of the same clone
# with one job off, and the stock arm is that clone with none. The clone, the template and the
# set are deleted at the end (BENCH_KEEP_DEVICES=1 keeps them); nothing else is touched.
#
# Defaults: --rounds 5, --idle 60, --settle-cpu simee's (foreground), --device-type "iPhone 17",
# --runtime the newest iOS runtime, and the candidates below. --quick: one round, --idle 10.
# Measures the jobs of #199's research that the slim profile doesn't turn off; the slim table's
# own entries can be measured the same way (--candidates "com.apple.linkd ..."), to fill a
# `saves` the table leaves empty. docs/ios.md "Launchd job profiles"; the table is
# crates/sim-core/src/launchd_profiles.rs.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
BENCH_ROUNDS=5
# Seen in a stock iOS 27.2 boot and not in the slim profile (#199): remotemanagementd and the
# declarative-device-management subscribers it starts (no MDM on a test device), the AppSSO pair,
# Health and FinanceKit's content daemons (healthd and financed are off already), Screen Time's
# extraction service (ScreenTimeAgent and UsageTrackingAgent are off already), and
# AuthenticationServicesAgent, the biggest job the profile keeps on purpose (AutoFill, passkeys:
# #223's keep-list idea). Bare names: each runtime's label is read from the clone.
candidates="remotemanagementd AppSSODaemon GSSCred healthcontentd finhealthd STExtractionService AuthenticationServicesAgent"
idle=""
settle_cpu=""
type="iPhone 17"
runtime=""
while [ $# -gt 0 ]; do
  case $1 in
    --candidates)
      candidates=$2
      shift 2
      ;;
    --idle)
      idle=$2
      shift 2
      ;;
    --settle-cpu)
      settle_cpu=$2
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
if [ -z "$idle" ]; then if [ "$BENCH_QUICK" = 1 ]; then idle=10; else idle=60; fi; fi
case $idle in '' | *[!0-9]*) bench_die "--idle takes whole seconds, not '$idle'" ;; *) ;; esac
[ "$idle" -gt 0 ] || bench_die "--idle must be at least 1 s"
case ${settle_cpu:-foreground} in foreground | all) ;; *) bench_die "--settle-cpu is foreground or all" ;; esac
ios_env
runtime=${runtime:-$(ios_runtime)}
[ -n "$runtime" ] || bench_skip "no iOS simulator runtime installed"
bench_start ios-slim
simee_ready
export SIMEE_IN_PROCESS=1

set_dir=$(mktemp -d "${TMPDIR:-/tmp}/simee-bench-slim-set.XXXXXX") || bench_die "mktemp failed"
sim() { "$BENCH_SIMEE" --set "$set_dir" "$@"; }
is_udid() { case $1 in ????????-????-????-????-????????????) return 0 ;; *) return 1 ;; esac }
# Shut down one of this run's devices and wait until the set says so.
slim_shutdown() {
  local i
  sim shutdown "$1" >/dev/null 2>&1
  for i in $(seq 1 60); do
    sim list --platform ios 2>/dev/null | grep -qF "($1) (Shutdown)" && return 0
    sleep 1
  done
  return 1
}
slim_delete() {
  sim list --platform ios 2>/dev/null | grep -qF "($1)" || return 0
  [ -n "${BENCH_KEEP_DEVICES:-}" ] && {
    bench_log "kept simulator $1 in $set_dir"
    return 0
  }
  slim_shutdown "$1"
  sim delete "$1" >/dev/null && bench_log "deleted simulator $1"
}
# The set's directory goes only when no device directory is left in it (a delete that failed
# keeps its device, and the set with it).
set_remove() {
  [ -n "${BENCH_KEEP_DEVICES:-}" ] && return 0
  if [ -z "$(find "$set_dir" -mindepth 1 -maxdepth 1 -type d 2>/dev/null)" ]; then
    rm -rf "$set_dir"
  else
    bench_log "kept $set_dir: a device in it wasn't deleted"
  fi
}
bench_on_exit set_remove

template=$(sim create "simee-bench-slim-template-$$" "$type" "$runtime" 2>"$BENCH_WORK/create.err" | tail -1)
is_udid "$template" || {
  cat "$BENCH_WORK/create.err" >&2
  bench_die "simee create '$type' $runtime failed"
}
bench_on_exit slim_delete "$template"
bench_log "created template $template in $set_dir"
sim boot "$template" --settle --timeout 600 >&2 || bench_die "the template didn't boot"
slim_shutdown "$template" || bench_die "the template didn't shut down"
clone=$(sim clone "$template" "simee-bench-slim-$$" 2>"$BENCH_WORK/clone.err" | tail -1)
is_udid "$clone" || {
  cat "$BENCH_WORK/clone.err" >&2
  bench_die "simee clone failed"
}
bench_on_exit slim_delete "$clone"
bench_refuse_protected "$template" "$clone"
bench_log "cloned $clone"
sim boot "$clone" --settle --timeout 600 >&2 || bench_die "the clone's first boot failed"

# resolve NAME LIST: the label NAME stands for in a `launchctl list` (PID, status, label): itself
# when it is one, else the one label whose last dot-separated part is NAME (any case). Status 1:
# none; 2: several (printed).
resolve() {
  awk -v want="$1" '
    NR > 1 && NF >= 3 {
      l = $3; n = split(l, part, ".")
      if (l == want) exact = l
      else if (index(want, ".") == 0 && tolower(part[n]) == tolower(want)) m[++k] = l
    }
    END {
      if (exact != "") { print exact; exit 0 }
      if (k == 1) { print m[1]; exit 0 }
      for (i = 1; i <= k; i++) printf "%s%s", (i > 1 ? " " : ""), m[i]
      if (k > 1) { print ""; exit 2 }
      exit 1
    }' <<<"$2"
}
listed=$(sim exec "$clone" launchctl list 2>/dev/null)
[ "$(printf '%s\n' "$listed" | wc -l)" -gt 10 ] || bench_die "launchctl list in the clone printed no jobs"
labels=()
skipped=()
for c in $candidates; do
  if l=$(resolve "$c" "$listed"); then
    labels+=("$l")
    bench_log "candidate $c: $l"
  elif [ -n "$l" ]; then
    skipped+=("$c (several: $l)")
    bench_log "candidate $c names several jobs ($l); give the label"
  else
    skipped+=("$c (not a job)")
    bench_log "candidate $c is not a job in $runtime; skipped"
  fi
done
[ ${#labels[@]} -gt 0 ] || bench_die "no candidate is a job in $runtime"

# The clone's launchd_sim tree in a `ps -Ao pid=,ppid=,time=,args=` snapshot, one "pid cpu-s"
# line per process; status 1 when the clone has no launchd_sim.
tree_cpu() {
  awk -v udid="/$1/" '
    function secs(t,   p, n, s, i, d) { d = 0; if (index(t, "-")) { split(t, p, "-"); d = p[1]; t = p[2] }
      n = split(t, p, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + p[i]; return d * 86400 + s }
    { pid = $1; parent[pid] = $2; cpu[pid] = secs($3); pids[++np] = pid
      if (($4 == "launchd_sim" || $4 ~ /\/launchd_sim$/) && index($0, udid)) root = pid }
    END {
      if (root == "") exit 1
      member[root] = 1; grown = 1
      while (grown) { grown = 0; for (i = 1; i <= np; i++) { p = pids[i]; if (!(p in member) && (parent[p] in member)) { member[p] = 1; grown = 1 } } }
      for (p in member) printf "%s %.2f\n", p, cpu[p]
    }' "$2"
}
# idle_window ARM: the clone's CPU over the next $idle s (% of a core), then its footprint (MB)
# and process count, as samples of ARM.
idle_window() {
  local a="$BENCH_WORK/ps-a" b="$BENCH_WORK/ps-b" pct fp n pids
  ps -Ao pid=,ppid=,time=,args= >"$a"
  sleep "$idle"
  ps -Ao pid=,ppid=,time=,args= >"$b"
  if ! tree_cpu "$clone" "$b" >"$BENCH_WORK/tree-b"; then
    bench_sample "$1" "CPU $idle s after settling, % of a core" "no-launchd_sim"
    return
  fi
  tree_cpu "$clone" "$a" >"$BENCH_WORK/tree-a" || : >"$BENCH_WORK/tree-a"
  pct=$(awk -v s="$idle" 'FNR == NR { before[$1] = $2; next } { d = $2 - ((($1) in before) ? before[$1] : 0); if (d > 0) t += d }
    END { printf "%.1f\n", 100 * t / s }' "$BENCH_WORK/tree-a" "$BENCH_WORK/tree-b")
  n=$(wc -l <"$BENCH_WORK/tree-b" | tr -d ' ')
  pids=$(awk '{ printf "%s ", $1 }' "$BENCH_WORK/tree-b")
  # shellcheck disable=SC2086 # one argument per PID
  fp=$(footprint --noCategories $pids 2>/dev/null | awk '/Summary Footprint:/ { v = $3; if ($4 == "GB") v *= 1024; if ($4 == "KB") v /= 1024; printf "%d\n", v }')
  bench_sample "$1" "CPU $idle s after settling, % of a core" "$pct"
  bench_sample "$1" "footprint after settling, MB" "${fp:-none}"
  bench_sample "$1" "processes after settling" "$n"
}
field() { printf ' %s\n' "$2" | sed -n "s/.* $1=\([^ ]*\).*/\1/p"; }
# measure ARM [LABEL]: one boot of the clone with LABEL off (none for the stock arm).
measure() {
  local arm=$1 label=${2:-} line extra=() dis run
  [ -n "$label" ] && sim exec "$clone" launchctl disable "system/$label" >/dev/null 2>&1
  if ! slim_shutdown "$clone"; then
    bench_sample "$arm" "launch until settled, s" "no-shutdown"
    return
  fi
  [ -n "$settle_cpu" ] && extra=(--settle-cpu "$settle_cpu")
  line=$(sim boot "$clone" --settle --timeout 600 ${extra[@]+"${extra[@]}"} 2>"$BENCH_WORK/boot.err" | tail -1)
  bench_log "$arm: $line"
  if [ "$(field settled "$line")" = true ]; then
    bench_sample "$arm" "launch until settled, s" "$(field settle_s "$line")"
    bench_sample "$arm" "CPU-s until settled" "$(field cpu_s "$line")"
  else
    bench_sample "$arm" "launch until settled, s" "unsettled"
  fi
  idle_window "$arm"
  if [ -n "$label" ]; then
    # After the idle window: each exec is a spawn in the device.
    dis=$(sim exec "$clone" launchctl print-disabled system 2>/dev/null)
    run=$(sim exec "$clone" launchctl list 2>/dev/null | awk -v l="$label" '$3 == l && $1 ~ /^[0-9]+$/')
    if grep -qE "\"$label\" => (disabled|true)" <<<"$dis" && [ -z "$run" ]; then
      bench_sample "$arm" "disable held" yes
    else
      bench_sample "$arm" "disable held" no
    fi
    sim exec "$clone" launchctl enable "system/$label" >/dev/null 2>&1
  fi
}
arms=(stock ${labels[@]+"${labels[@]}"})
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  order=("${arms[@]}")
  if [ $((BENCH_ROUND % 2)) = 0 ]; then
    order=()
    for ((i = ${#arms[@]} - 1; i >= 0; i--)); do order+=("${arms[i]}"); done
  fi
  for arm in "${order[@]}"; do
    if [ "$arm" = stock ]; then measure stock; else measure "$arm" "$arm"; fi
  done
done
note="One clone of a booted-once $type ($runtime) template in its own device set, warm reboots with one launchd job disabled (stock: none); simee boot --settle${settle_cpu:+ --settle-cpu $settle_cpu}; CPU, footprint and processes of its launchd_sim tree over the $idle s after it settled."
[ ${#skipped[@]} -gt 0 ] && note="$note Not measured: $(printf '%s, ' "${skipped[@]}" | sed 's/, $//')."
bench_finish "$(
  IFS=,
  echo "${arms[*]}"
)" "$note"
