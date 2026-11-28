#!/usr/bin/env bash
# The quiet-host gate every scripts/bench/ benchmark passes before it measures, and the host facts
# its record carries. Sourced by lib.sh; run directly it is also a command:
#
#   scripts/bench/quiet.sh gate     wait (up to BENCH_QUIET_WAIT s) for a quiet host; exit 0 and print
#                                   how it passed, or print what is still busy and exit 4
#   scripts/bench/quiet.sh check    one look, no waiting: print what is busy; exit 4 when anything is
#   scripts/bench/quiet.sh facts    key=value lines: os, ncpu, load1, load_limit, thermal,
#                                   thermal_detail, power, laptop (the host record reads these)
#
# A host is quiet when all of these hold:
#   - the 1-minute load is below BENCH_QUIET_LOAD_FACTOR x CPUs (default 0.5), or below
#     BENCH_QUIET_MAX_LOAD when that is set;
#   - no emulator, simulator or compiler process is running that this run didn't start. Own
#     processes are this shell, its ancestors, its descendants, BENCH_QUIET_ALLOW_PIDS (and their
#     descendants). Processes are matched by name to *detect* them; nothing is ever signalled;
#   - no thermal pressure: macOS `pmset -g therm` (CPU_Speed_Limit / CPU_Scheduler_Limit under 100)
#     and, when it can run without a prompt (root, or `sudo -n`), `powermetrics --samplers thermal`
#     (a pressure level other than Nominal); Linux /sys/class/thermal (a zone at or over
#     BENCH_QUIET_MAX_TEMP_C, default 85, or a processor cooling device above state 0). A host with
#     no sensors (a VM, a container) is recorded as "unavailable" and passes;
#   - no battery power: macOS `pmset -g batt`, Linux /sys/class/power_supply. A host that reports
#     no power source passes.
#
# Knobs: BENCH_QUIET_WAIT (300 s), BENCH_QUIET_POLL (5 s between looks). For tests, BENCH_QUIET_OS,
# BENCH_QUIET_PROC (/proc), BENCH_QUIET_SYS (/sys), BENCH_QUIET_SELF_PID, with ps, sysctl, pmset
# and uname found on PATH. Works on macOS's bash 3.2 (no associative arrays, no mapfile).
# docs/research/bench-protocol.md.

quiet_os() { echo "${BENCH_QUIET_OS:-$(uname -s)}"; }

quiet_ncpu() {
  if [ "$(quiet_os)" = Darwin ]; then
    sysctl -n hw.ncpu 2>/dev/null || getconf _NPROCESSORS_ONLN
  else
    getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu
  fi
}

quiet_load1() {
  if [ "$(quiet_os)" = Darwin ]; then
    sysctl -n vm.loadavg 2>/dev/null | awk '{ gsub(/[{}]/, ""); print $1 }'
  else
    cut -d' ' -f1 "${BENCH_QUIET_PROC:-/proc}/loadavg" 2>/dev/null
  fi
}

# The load the host must be under: BENCH_QUIET_MAX_LOAD, else BENCH_QUIET_LOAD_FACTOR x CPUs.
quiet_load_limit() {
  if [ -n "${BENCH_QUIET_MAX_LOAD:-}" ]; then
    echo "$BENCH_QUIET_MAX_LOAD"
  else
    awk -v f="${BENCH_QUIET_LOAD_FACTOR:-0.5}" -v n="$(quiet_ncpu)" 'BEGIN { printf "%.2f\n", f * n }'
  fi
}

# --- thermal -----------------------------------------------------------------------------------

# Prints "state|detail" for macOS: state is nominal, pressure or unavailable.
quiet_thermal_darwin() {
  local detail="" state=nominal pm lim pmo lvl
  if pm=$(pmset -g therm 2>/dev/null) && [ -n "$pm" ]; then
    lim=$(printf '%s\n' "$pm" | awk '
      /CPU_Speed_Limit|CPU_Scheduler_Limit/ {
        v = $0; sub(/^[^=]*=[ \t]*/, "", v); sub(/[^0-9].*$/, "", v)
        if (v != "" && v + 0 < 100) { name = $1; low = low (low == "" ? "" : ", ") name "=" v }
      }
      /Note:.*(thermal|performance) warning level/ && !/No (thermal|performance)/ { low = low (low == "" ? "" : ", ") "warning level recorded" }
      END { print low }')
    if [ -n "$lim" ]; then
      state=pressure
      detail="pmset: $lim"
    else
      detail="pmset: no limit"
    fi
  else
    detail="pmset: no thermal data"
    state=unavailable
  fi
  # powermetrics needs root. Use it when it won't prompt; otherwise say it wasn't read.
  pmo=""
  if [ "$(id -u)" = 0 ]; then
    pmo=$(powermetrics --samplers thermal -n 1 -i 100 2>/dev/null)
  elif sudo -n true 2>/dev/null; then
    pmo=$(sudo -n powermetrics --samplers thermal -n 1 -i 100 2>/dev/null)
  fi
  lvl=$(printf '%s\n' "$pmo" | sed -n 's/^[[:space:]]*Current pressure level:[[:space:]]*//p' | head -1)
  if [ -n "$lvl" ]; then
    detail="$detail; powermetrics: $lvl"
    if [ "$lvl" != Nominal ]; then state=pressure; elif [ "$state" = unavailable ]; then state=nominal; fi
  else
    detail="$detail; powermetrics: not read (needs root)"
  fi
  echo "$state|$detail"
}

# Prints "state|detail" for Linux, from /sys/class/thermal.
quiet_thermal_linux() {
  local sys=${BENCH_QUIET_SYS:-/sys} max=${BENCH_QUIET_MAX_TEMP_C:-85} z t ty c cur mx found=0 hot=""
  for z in "$sys"/class/thermal/thermal_zone*; do
    [ -r "$z/temp" ] || continue
    t=$(cat "$z/temp" 2>/dev/null)
    case $t in '' | *[!0-9-]*) continue ;; *) ;; esac
    found=1
    ty=$(cat "$z/type" 2>/dev/null)
    # Millidegrees, but a driver that reports whole degrees is read as such.
    if awk -v t="$t" -v m="$max" 'BEGIN { if (t > 1000) t /= 1000; exit !(t >= m) }'; then
      hot="$hot${hot:+, }$(basename "$z") ${ty:-zone} $(awk -v t="$t" 'BEGIN { if (t > 1000) t /= 1000; printf "%.0f", t }') C"
    fi
  done
  for c in "$sys"/class/thermal/cooling_device*; do
    [ -r "$c/cur_state" ] || continue
    ty=$(cat "$c/type" 2>/dev/null)
    case $(printf %s "$ty" | tr '[:upper:]' '[:lower:]') in *processor* | *cpufreq* | *powerclamp* | *cpu*) ;; *) continue ;; esac
    found=1
    cur=$(cat "$c/cur_state" 2>/dev/null)
    mx=$(cat "$c/max_state" 2>/dev/null)
    case $cur in '' | *[!0-9]*) continue ;; *) ;; esac
    [ "$cur" -gt 0 ] && hot="$hot${hot:+, }$(basename "$c") $ty throttled ($cur of ${mx:-?})"
  done
  if [ -n "$hot" ]; then
    echo "pressure|$hot"
  elif [ "$found" = 1 ]; then
    echo "nominal|/sys/class/thermal: no zone at or over $max C, no processor throttled"
  else
    echo "unavailable|no /sys/class/thermal sensors (a VM or a container)"
  fi
}

quiet_thermal() {
  if [ "$(quiet_os)" = Darwin ]; then quiet_thermal_darwin; else quiet_thermal_linux; fi
}

# --- power -------------------------------------------------------------------------------------

# Prints "source|laptop": source is ac, battery, ups or unknown; laptop is 1 with an internal battery.
quiet_power() {
  if [ "$(quiet_os)" = Darwin ]; then
    pmset -g batt 2>/dev/null | awk '
      /Now drawing from/ { s = ($0 ~ /AC Power/) ? "ac" : ($0 ~ /Battery Power/) ? "battery" : ($0 ~ /UPS/) ? "ups" : "unknown" }
      /InternalBattery/ { lap = 1 }
      END { printf "%s|%d\n", (s == "" ? "unknown" : s), lap }'
  else
    local sys=${BENCH_QUIET_SYS:-/sys} d ty on st bat=0 ac=0 dis=0 chg=0
    for d in "$sys"/class/power_supply/*; do
      [ -r "$d/type" ] || continue
      ty=$(cat "$d/type" 2>/dev/null)
      if [ "$ty" = Battery ]; then
        [ "$(cat "$d/present" 2>/dev/null)" = 0 ] && continue
        bat=1
        st=$(cat "$d/status" 2>/dev/null)
        case $st in
          Discharging) dis=1 ;;
          Charging | Full | "Not charging") chg=1 ;;
          *) ;;
        esac
      else
        on=$(cat "$d/online" 2>/dev/null)
        case $on in '' | 0 | *[!0-9]*) ;; *) ac=1 ;; esac
      fi
    done
    if [ "$dis" = 1 ]; then
      echo "battery|1"
    elif [ "$ac" = 1 ] || [ "$chg" = 1 ]; then
      echo "ac|$bat"
    elif [ "$bat" = 1 ]; then
      echo "battery|1"
    else
      echo "unknown|0"
    fi
  fi
}

# --- foreign processes -------------------------------------------------------------------------

# Names (basename of the command) of the processes a benchmark must not share the host with.
# Emulators and VMs, simulators (and simee's own front ends), and compilers. Bracket expressions,
# not backslashes: awk -v would read `\.` and `\+` as escape sequences.
QUIET_RE_EMULATOR='^(emu|emulator|emulator64-.*|qemu-.*|crosvm|simee-emu)$'
QUIET_RE_SIMULATOR='^(Simulator|simee|simee-desktop|launchd_sim|SimulatorTrampoline|com[.]apple[.]CoreSimulator[.]CoreSimulatorService)$'
QUIET_RE_COMPILER='^(cargo|rustc|clang|clang[+][+]|clang-[0-9]+|gcc|g[+][+]|cc|c[+][+]|cc1|cc1plus|ld|lld|rust-lld|swiftc|swift-frontend|xcodebuild|ninja)$'

# quiet_foreign: one line per foreign process, "CLASS NAME PID". `ps -axo pid=,ppid=,comm=` is read
# once; the comm may be a path with spaces on macOS, so the rest of the line is the command.
quiet_foreign() {
  local self=${BENCH_QUIET_SELF_PID:-$$}
  ps -axo pid=,ppid=,comm= 2>/dev/null | awk -v self="$self" -v allow="${BENCH_QUIET_ALLOW_PIDS:-}" \
    -v re_e="$QUIET_RE_EMULATOR" -v re_s="$QUIET_RE_SIMULATOR" -v re_c="$QUIET_RE_COMPILER" '
    {
      pid = $1; ppid[pid] = $2
      cmd = $0; sub(/^[ \t]*[0-9]+[ \t]+[0-9]+[ \t]+/, "", cmd)
      n = split(cmd, parts, "/"); name[pid] = parts[n]
      order[++count] = pid
    }
    END {
      # Own: this shell and its ancestors (whoever started it), plus everything below this shell
      # and below the allowed PIDs. Not the ancestors'"'"' other children: those are foreign.
      for (p = self; p != "" && p != 0 && !(p in own); p = ppid[p]) own[p] = 1
      root[self] = 1
      na = split(allow, a, " ")
      for (i = 1; i <= na; i++) { own[a[i]] = 1; root[a[i]] = 1 }
      do {
        grew = 0
        for (i = 1; i <= count; i++) {
          p = order[i]
          if (!(p in below) && (ppid[p] in root || ppid[p] in below)) { below[p] = 1; own[p] = 1; grew = 1 }
        }
      } while (grew)
      for (i = 1; i <= count; i++) {
        p = order[i]
        if (p in own) continue
        cls = (name[p] ~ re_e) ? "emulator" : (name[p] ~ re_s) ? "simulator" : (name[p] ~ re_c) ? "compiler" : ""
        if (cls != "") print cls, name[p], p
      }
    }'
}

# --- the gate ----------------------------------------------------------------------------------

# quiet_reasons: what keeps the host from being quiet, one line each; nothing when it is quiet.
quiet_reasons() {
  local load limit ncpu th pw cls name pid
  load=$(quiet_load1)
  limit=$(quiet_load_limit)
  ncpu=$(quiet_ncpu)
  if [ -z "$load" ]; then
    echo "load: the 1-minute load could not be read"
  elif awk -v l="$load" -v m="$limit" 'BEGIN { exit !(l >= m) }'; then
    echo "load: the 1-minute load is $load, not below $limit (${BENCH_QUIET_LOAD_FACTOR:-0.5} x $ncpu CPUs; BENCH_QUIET_LOAD_FACTOR or BENCH_QUIET_MAX_LOAD)"
  fi
  while read -r cls name pid; do
    [ -n "$pid" ] && echo "process: $cls $name (pid $pid) is running"
  done < <(quiet_foreign)
  th=$(quiet_thermal)
  [ "${th%%|*}" = pressure ] && echo "thermal: ${th#*|}"
  pw=$(quiet_power)
  [ "${pw%%|*}" = battery ] && echo "power: on battery"
  return 0
}

# quiet_facts: key=value lines for the host record.
quiet_facts() {
  local th pw
  th=$(quiet_thermal)
  pw=$(quiet_power)
  echo "os=$(quiet_os)"
  echo "ncpu=$(quiet_ncpu)"
  echo "load1=$(quiet_load1)"
  echo "load_limit=$(quiet_load_limit)"
  echo "thermal=${th%%|*}"
  echo "thermal_detail=${th#*|}"
  echo "power=${pw%%|*}"
  echo "laptop=${pw#*|}"
}

# quiet_gate: waits up to BENCH_QUIET_WAIT s, looking every BENCH_QUIET_POLL s. Quiet: prints
# "passed" (or "passed after N s") on stdout, returns 0. Not quiet in time: prints what is busy
# on stderr, returns 4. Progress goes to stderr.
quiet_gate() {
  local wait=${BENCH_QUIET_WAIT:-300} poll=${BENCH_QUIET_POLL:-5} t0 reasons waited last=-1000
  t0=$(date +%s)
  while :; do
    reasons=$(quiet_reasons)
    waited=$(($(date +%s) - t0))
    if [ -z "$reasons" ]; then
      if [ "$waited" -le 0 ]; then echo passed; else echo "passed after $waited s"; fi
      return 0
    fi
    if [ "$waited" -ge "$wait" ]; then
      {
        echo "refused: this host is not quiet${BENCH_QUIET_WHO:+ for $BENCH_QUIET_WHO} (looked for $waited s, BENCH_QUIET_WAIT=$wait):"
        printf '%s\n' "$reasons" | sed 's/^/  /'
        echo "Run it when the host is idle, or loosen a limit with BENCH_QUIET_* (docs/research/bench-protocol.md)."
        echo "--no-quiet-gate runs anyway, for a smoke test whose numbers are not published."
      } >&2
      return 4
    fi
    if [ $((waited - last)) -ge 30 ]; then
      echo "[$(date +%H:%M:%S)] waiting for a quiet host ($waited of $wait s): $(printf '%s' "$reasons" | head -1)" >&2
      last=$waited
    fi
    sleep "$poll"
  done
}

# Run directly: gate, check or facts.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -uo pipefail
  case ${1:-} in
    gate)
      quiet_gate
      exit $?
      ;;
    check)
      reasons=$(quiet_reasons)
      [ -z "$reasons" ] && exit 0
      printf '%s\n' "$reasons"
      exit 4
      ;;
    facts)
      quiet_facts
      ;;
    *)
      awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
      [ "${1:-}" = -h ] || [ "${1:-}" = --help ] || exit 2
      ;;
  esac
fi
