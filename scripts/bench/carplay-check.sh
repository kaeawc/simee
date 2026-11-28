#!/usr/bin/env bash
# CarPlay per Xcode release (#441): does the simulator render CarPlay on each installed iOS runtime?
#
#   scripts/bench/carplay-check.sh [--simee BIN] [--device-type TYPE] [--runtime ID]...
#                                  [--min-ios N] [--settle SECONDS] [--attach-wait SECONDS]
#                                  [--out DIR] [--set DIR]
#
# For every installed iOS runtime of version --min-ios (default 26) or later that has the device
# type (default "iPhone 17"; a runtime without it, such as the iPhone Duo-only ones, gets its first
# iPhone device type), in a device set of its own (`simee --set`, so nothing lands in the
# default set and Simulator.app opens no window): creates an iPhone, boots it, waits --settle
# seconds (default 20), calls `simee carplay <device> on`, waits --attach-wait seconds (default 10),
# takes `screenshot --display carplay`,
# and judges the screenshot: it renders when more than 1% of its pixels are not black. Prints a
# Markdown table (date, Xcode build, runtime, whether the call worked, renders yes/no) and writes
# it and the screenshots to --out (default bench/results/carplay-<date>). Paste the rows into
# docs/ios-versions.md ("CarPlay, per Xcode release").
#
# simee refuses CarPlay on an iOS 27 runtime (`sim_core::carplay::runtime_supports`); this script
# sets SIMEE_CARPLAY_UNGATED=1 for the call so the runtime is tried anyway, since the gate is what
# the check exists to re-test. If a release renders on iOS 27, drop the gate and update its test.
#
# CARPLAY_BOOT_WRAP, when set, is a command prefix for each boot (a host-wide boot gate, e.g.
# "boot-slot.sh --"). Creates its own simulators and set and deletes them on exit; touches no
# other device and never opens or quits Simulator.app. macOS with Xcode only.
set -uo pipefail
. "$(dirname "$0")/lib.sh"
type="iPhone 17"
min_ios=26
settle=20
attach_wait=10
set_dir=""
runtimes=()
out=""
while [ $# -gt 0 ]; do
  case $1 in
    --device-type)
      type=$2
      shift 2
      ;;
    --runtime)
      runtimes+=("$2")
      shift 2
      ;;
    --min-ios)
      min_ios=$2
      shift 2
      ;;
    --settle)
      settle=$2
      shift 2
      ;;
    --attach-wait)
      attach_wait=$2
      shift 2
      ;;
    --set)
      set_dir=$2
      shift 2
      ;;
    --out)
      out=$2
      shift 2
      ;;
    --simee)
      BENCH_SIMEE=$2
      shift 2
      ;;
    -h | --help)
      bench_help
      exit 0
      ;;
    *)
      echo "carplay-check.sh: unknown argument $1" >&2
      exit 2
      ;;
  esac
done
ios_env
simee_ready
date_s=$(date +%Y-%m-%d)
xcode=$(xcodebuild -version 2>/dev/null | tr '\n' ' ' | sed 's/ *$//')
xcode=${xcode:-unknown}
out=${out:-$BENCH_REPO/bench/results/carplay-$date_s}
mkdir -p "$out" || exit 1
own_set=0
if [ -z "$set_dir" ]; then
  set_dir=$(mktemp -d "${TMPDIR:-/tmp}/carplay-check.XXXXXX") || exit 1
  own_set=1
fi
if [ "$own_set" = 0 ] && [ -e "$set_dir" ]; then
  echo "carplay-check.sh: $set_dir exists; give a directory this run creates" >&2
  exit 2
fi
udids=()
cleanup() {
  local u
  for u in ${udids[@]+"${udids[@]}"}; do
    SIMEE_IN_PROCESS=1 "$BENCH_SIMEE" --set "$set_dir" shutdown "$u" >/dev/null 2>&1
    SIMEE_IN_PROCESS=1 "$BENCH_SIMEE" --set "$set_dir" delete "$u" >/dev/null 2>&1
  done
  rm -rf "$set_dir"
}
trap cleanup EXIT
sim() { SIMEE_IN_PROCESS=1 "$BENCH_SIMEE" --set "$set_dir" "$@"; }

# Runtimes to check: "identifier<TAB>name<TAB>build<TAB>device type" (type empty: no iPhone type).
list=$(xcrun simctl list runtimes -j | python3 -I -c '
import json, sys
t, lo, want = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
for r in json.load(sys.stdin)["runtimes"]:
    if not r.get("isAvailable", True) or ".iOS-" not in r["identifier"]:
        continue
    if want and r["identifier"] not in want and r["identifier"].rsplit(".", 1)[-1] not in want:
        continue
    if not want and int(r["version"].split(".")[0]) < lo:
        continue
    names = [d["name"] for d in r.get("supportedDeviceTypes", [])]
    pick = t if t in names else next((n for n in names if n.startswith("iPhone")), "")
    print("%s\t%s\t%s\t%s" % (r["identifier"], r["name"], r["buildversion"], pick))
' "$type" "$min_ios" ${runtimes[@]+"${runtimes[@]}"}) || exit 1
[ -n "$list" ] || {
  echo "carplay-check.sh: no iOS runtime of version $min_ios or later installed" >&2
  exit 1
}

# judge PNG: "yes" when more than 1% of the pixels are not black, with the share.
judge() {
  python3 -I -c '
import sys
try:
    from PIL import Image
except ImportError:
    print("unknown (no PIL)")
    raise SystemExit
im = Image.open(sys.argv[1]).convert("L")
h = im.histogram()
lit = sum(h[17:]) / max(1, sum(h))
print("%s (%.1f%% of pixels lit)" % ("yes" if lit > 0.01 else "no", lit * 100))
' "$1"
}

rows=""
n=0
while IFS=$'\t' read -r id name build dtype; do
  n=$((n + 1))
  short=${id##*.}
  if [ -z "$dtype" ]; then
    rows="$rows| $date_s | $xcode | $name ($build) | no iPhone device type on this runtime | n/a |
"
    continue
  fi
  dev="carplay-check-$$-$n"
  if ! udid=$(sim create "$dev" "$dtype" "$id" 2>"$out/$short.log"); then
    rows="$rows| $date_s | $xcode | $name ($build) | create failed (see $short.log) | n/a |
"
    continue
  fi
  udid=$(printf %s "$udid" | tail -1)
  udids+=("$udid")
  # shellcheck disable=SC2086 # the wrap is a command prefix, split on purpose
  if ! SIMEE_IN_PROCESS=1 ${CARPLAY_BOOT_WRAP:-} "$BENCH_SIMEE" --set "$set_dir" boot "$udid" --timeout 600 >>"$out/$short.log" 2>&1; then
    rows="$rows| $date_s | $xcode | $name ($build) | boot failed (see $short.log) | n/a |
"
    continue
  fi
  sleep "$settle"
  call=ok
  if ! SIMEE_CARPLAY_UNGATED=1 sim carplay "$udid" on >>"$out/$short.log" 2>&1; then
    call="refused: $(tail -1 "$out/$short.log" | cut -c1-120)"
  fi
  sleep "$attach_wait"
  png="$out/$short.png"
  if sim screenshot "$udid" "$png" --display carplay >>"$out/$short.log" 2>&1; then
    verdict=$(judge "$png")
  else
    verdict="no (no CarPlay screenshot)"
  fi
  rows="$rows| $date_s | $xcode | $name ($build), $dtype | $call | $verdict |
"
  sim carplay "$udid" off >/dev/null 2>&1
  sim shutdown "$udid" >>"$out/$short.log" 2>&1
  sim delete "$udid" >>"$out/$short.log" 2>&1
  udids=()
done <<<"$list"

{
  echo "| Date | Xcode | Runtime | \`carplay on\` | Renders |"
  echo "| --- | --- | --- | --- | --- |"
  printf %s "$rows"
} | tee "$out/carplay-check.md"
echo "screenshots and logs: $out" >&2
