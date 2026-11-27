#!/usr/bin/env bash
# Every published benchmark, by name, one after another (never two devices at once).
#
#   scripts/bench/run-all.sh --list                      the names, what each needs, its command
#   scripts/bench/run-all.sh [--only NAME[,NAME]] [--kind host|android|ios|linux] [--quick]
#                            [--rounds N] [--out DIR]
#
# Without --only or --kind it runs all of them, which takes hours and boots devices (one at a
# time, except the farms). Each benchmark creates and deletes its own devices; a benchmark this
# host can't run (no SDK emulator, no iOS runtime, not Linux) is reported as skipped. --quick
# passes --quick to each (one short round: a smoke test, not a number to publish). Results: one
# JSON per benchmark in bench/results/ (or --out), and each one's Markdown table on stdout.
# docs/benchmarks.md names the benchmark behind every published number.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# name|kind|command (relative to scripts/bench)|docs/benchmarks.md section
# RUN_ALL_REGISTRY=FILE (with RUN_ALL_DIR for the scripts) replaces the list: scripts/bench/test-run-all.sh.
if [ -n "${RUN_ALL_REGISTRY:-}" ]; then here=${RUN_ALL_DIR:-$here}; fi
registry() {
  if [ -n "${RUN_ALL_REGISTRY:-}" ]; then
    cat "$RUN_ALL_REGISTRY"
    return
  fi
  cat <<'EOF'
android-boot|android|android-boot.sh|Android: emu against the SDK emulator
android-boot-api29|android|android-boot.sh --image system-images/android-29/default/arm64-v8a --lcd 720x1280@320|API 29 (GLES over the goldfish pipe)
android-idle-api29|android|android-idle.sh|API 29 and 30: first boots and idle host CPU
android-idle-api30|android|android-idle.sh --image system-images/android-30/google_apis/arm64-v8a|API 29 and 30: first boots and idle host CPU
android-gles-first|android|android-idle.sh --arms "emu emu:ANGLE_DEFAULT_PLATFORM=metal" --launcher --window 0|API 29 (GLES over the goldfish pipe), first boots by host GLES
android-gles-warm|android|android-idle.sh --warm --arms "emu emu:ANGLE_DEFAULT_PLATFORM=swiftshader" --launcher --window 0|API 29 (GLES over the goldfish pipe), warm boots by host GLES
android-frames-phone|android|android-frames.sh --device phone|Frame pacing
android-frames-tablet|android|android-frames.sh --device tablet|Frame pacing
android-renderers|android|renderers.sh|Android renderers (docs/research/renderers.md)
android-net|android|android-net.sh|Idle host CPU (the NAT) and NAT throughput
android-reset|android|android-reset.sh|Emulator internals: guest resets
android-steps|android|api-steps.sh --android|Software mode through emu -avd (tap, screenshot)
android-farm|android|android-farm.sh|Android farm
android-bundle|android|android-bundle.sh|Android: device bundles
software-mode|linux|software-mode.sh|Boot to sys.boot_completed with emu's devices
encode-linux|linux|encode-linux.sh|Video encode (H.264, OpenH264 on Linux)
software-profile|linux|software-profile.sh|Software mode: a smaller screen and 30 Hz
ios-boot|ios|ios-boot.sh|iOS: simee against simctl (boots)
ios-command|ios|ios-command.sh|iOS: simee against simctl (one screenshot as a command)
ios-steps|ios|api-steps.sh|iOS: simee against simctl (time per automation step)
ios-farm|ios|ios-farm.sh|iOS: simee against simctl (farm, default and slim)
ios-settle|ios|ios-settle.sh|The slim profile, one clone at a time
ios-api-lookup|ios|ios-api.py --suite lookup|Per-call device lookup on a crowded host
ios-api-introspection|ios|ios-api.py --suite introspection|Introspection
ios-api-launch|ios|ios-api.py --suite launch|launch, with and without restart
ios-api-input|ios|ios-api.py --suite input|iOS input defaults (timings)
ios-api-pointer|ios|ios-api.py --suite pointer|iPad pointer click (timings)
ios-api-prefs|ios|ios-api.py --suite prefs|iOS app preferences
ios-api-mem|ios|ios-api.py --suite mem|iOS memory tools
host-paths|host|host-paths.sh|Emulator internals (device-free rows)
build-times|host|build-times.sh|Build times (local)
mcp-build|host|mcp-build.sh|MCP server
EOF
}
only=""
kind=""
pass=()
while [ $# -gt 0 ]; do
  case $1 in
    -h | --help)
      awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
      exit 0
      ;;
    --list)
      printf '%-24s %-8s %s\n' NAME NEEDS COMMAND
      registry | while IFS='|' read -r name k cmd _; do printf '%-24s %-8s scripts/bench/%s\n' "$name" "$k" "$cmd"; done
      exit 0
      ;;
    --only)
      only=$2
      shift 2
      ;;
    --kind)
      kind=$2
      shift 2
      ;;
    --quick)
      pass+=(--quick)
      shift
      ;;
    --rounds | --out)
      pass+=("$1" "$2")
      shift 2
      ;;
    *)
      echo "run-all.sh: unknown option $1 (--help)" >&2
      exit 2
      ;;
  esac
done
if [ -n "$only" ]; then
  for n in ${only//,/ }; do registry | grep -q "^$n|" || {
    echo "run-all.sh: no benchmark $n (--list)" >&2
    exit 2
  }; done
fi
summary=()
failed=0
interrupted=0
while IFS='|' read -r name k cmd section; do
  [ -n "$kind" ] && [ "$k" != "$kind" ] && continue
  if [ -n "$only" ]; then case ",$only," in *",$name,"*) ;; *) continue ;; esac fi
  echo "== $name ($section): scripts/bench/$cmd ${pass[*]-}" >&2
  # The registry's commands are plain words and double-quoted --arms values.
  eval "set -- $cmd"
  script=$1
  shift
  case $script in
    *.py)
      python3 -I "$here/$script" "$@" ${pass[@]+"${pass[@]}"} </dev/null
      rc=$?
      ;;
    *)
      "$here/$script" "$@" ${pass[@]+"${pass[@]}"} </dev/null
      rc=$?
      ;;
  esac
  case $rc in
    0) summary+=("ok       $name") ;;
    3) summary+=("skipped  $name") ;;
    # 130 and 143: the child was interrupted (lib.sh turns SIGINT and SIGTERM into these exits).
    # Stop the whole run: the next benchmark would boot another device the user asked to stop.
    130 | 143)
      summary+=("STOPPED  $name (exit $rc); the remaining benchmarks were not run")
      interrupted=$rc
      break
      ;;
    *)
      summary+=("FAILED   $name (exit $rc)")
      failed=1
      ;;
  esac
done < <(registry)
printf '%s\n' "" "run-all:" "${summary[@]}" >&2
[ "$interrupted" = 0 ] || exit "$interrupted"
exit "$failed"
