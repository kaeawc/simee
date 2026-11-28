#!/usr/bin/env bash
# Tests for ios-slim.sh with no simulator: a fake simee (its device set, devices, boots and the
# clone's launchctl), a fake ps that shows the clone's launchd_sim tree, a fake footprint, and an
# xcrun that answers only `simctl help` stand in for the host, on macOS and Linux alike. Covered:
#   - every simee call names one device set, a new directory the run made, and the run removes it;
#   - the template and the clone are created, booted, and deleted at the end, clone first;
#   - candidates resolve against the clone's `launchctl list`: a label as given, a bare name as the
#     one label it ends in; one that names several jobs, or none, is reported and not measured;
#   - each candidate alone is disabled before a reboot and enabled after it, the stock arm touches
#     none, and the order reverses on even rounds;
#   - the result JSON has the arms, the medians (settle, CPU over the idle window from the ps
#     snapshots, footprint, processes, whether the disable held) and the spreads.
#
#   scripts/bench/test-ios-slim.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/test-ios-slim.XXXXXX")
trap 'rm -rf "$work"' EXIT
fails=0
check() { # description, condition result
  if [ "$2" = ok ]; then echo "ok   $1"; else
    echo "FAIL $1"
    fails=$((fails + 1))
  fi
}
yes_if() { if "$@"; then echo ok; fi; }

mkdir -p "$work/bin" "$work/state" "$work/tmp" "$work/out"
stub() { # name, body
  printf '#!/usr/bin/env bash\n%s\n' "$2" >"$work/bin/$1"
  chmod +x "$work/bin/$1"
}
T=11111111-1111-1111-1111-111111111111
C=22222222-2222-2222-2222-222222222222
# shellcheck disable=SC2016 # the stubs expand their own variables when they run
stub uname 'if [ "${1:-}" = -s ]; then echo Darwin; else exec /usr/bin/env -i PATH=/usr/bin:/bin uname "$@"; fi'
# shellcheck disable=SC2016
stub xcrun 'echo "xcrun $*" >>"$FAKE_STATE/xcrun"; [ "$*" = "simctl help" ]'
stub footprint 'echo "Summary Footprint: 812 MB"'
# The clone's tree: launchd_sim and a daemon, and a process that starts between the snapshots.
# shellcheck disable=SC2016
stub ps 'if [ "$*" != "-Ao pid=,ppid=,time=,args=" ]; then PATH=/usr/bin:/bin exec ps "$@"; fi
n=$(cat "$FAKE_STATE/ps" 2>/dev/null || echo 0); echo $((n + 1)) >"$FAKE_STATE/ps"
if [ $((n % 2)) = 0 ]; then
  printf "500 1 0:01.00 /rt/sbin/launchd_sim /sets/x/'"$C"'/data/var/run/launchd_bootstrap.plist\n501 500 0:02.00 /rt/usr/libexec/d\n"
else
  printf "500 1 0:01.50 /rt/sbin/launchd_sim /sets/x/'"$C"'/data/var/run/launchd_bootstrap.plist\n501 500 0:03.00 /rt/usr/libexec/d\n502 500 0:00.50 /rt/usr/libexec/new\n"
fi'
cat >"$work/bin/simee" <<EOF
#!/usr/bin/env bash
s=\$FAKE_STATE
[ "\$1" = --set ] || { echo "no --set: \$*" >&2; exit 9; }
echo "\$2" >>"\$s/sets"
set_dir=\$2
shift 2
echo "\$*" >>"\$s/calls"
dev() { echo "\$2 \$1 \$3" >"\$s/dev-\$1"; }
state() { read -r _ n _ <"\$s/dev-\$1"; dev "\$1" "\$n" "\$2"; }
case \$1 in
  create) dev $T "\$2" Shutdown; mkdir -p "\$set_dir/$T"; echo $T ;;
  clone) dev $C "\$3" Shutdown; mkdir -p "\$set_dir/$C"; echo $C ;;
  boot) state "\$2" Booted; echo "device=\$2 boot_s=5.0 settled=true settle_s=31.5 procs=120 cpu_s=80.2" ;;
  shutdown) state "\$2" Shutdown ;;
  delete) rm -f "\$s/dev-\$2"; rm -rf "\${set_dir:?}/\$2" ;;
  list) for f in "\$s"/dev-*; do [ -e "\$f" ] && { read -r u n st <"\$f"; echo "    \$n (\$u) (\$st) [iOS]"; }; done ;;
  exec)
    [ "\$3" = launchctl ] || exit 9
    case \$4 in
      list)
        printf 'PID\tStatus\tLabel\n'
        for l in SpringBoard GSSCred remotemanagementd a.dup b.dup j1 j2 j3 j4 j5 j6 j7 j8; do
          pid=12; grep -qxF "com.apple.\$l" "\$s/disabled" 2>/dev/null && pid=-
          printf '%s\t0\tcom.apple.%s\n' "\$pid" "\$l"
        done ;;
      print-disabled) echo "disabled services = {"; while read -r l; do echo "  \"\$l\" => disabled"; done < <(cat "\$s/disabled" 2>/dev/null); echo "}" ;;
      disable) echo "\${5#system/}" >>"\$s/disabled" ;;
      enable) grep -vxF "\${5#system/}" "\$s/disabled" >"\$s/d.tmp"; mv "\$s/d.tmp" "\$s/disabled" ;;
      *) exit 9 ;;
    esac ;;
  *) echo "fake simee: \$1?" >&2; exit 9 ;;
esac
EOF
chmod +x "$work/bin/simee"

out=$(FAKE_STATE="$work/state" PATH="$work/bin:$PATH" TMPDIR="$work/tmp" BENCH_TRACK_WAIT=1 \
  "$here/ios-slim.sh" --simee "$work/bin/simee" --runtime com.apple.CoreSimulator.SimRuntime.iOS-27-2 --no-quiet-gate \
  --rounds 2 --idle 2 --candidates "GSSCred com.apple.remotemanagementd dup nosuch" --out "$work/out" 2>"$work/err")
rc=$?
check "the run succeeds (exit $rc)" "$([ $rc = 0 ] && echo ok)"
[ $rc = 0 ] || tail -20 "$work/err"
calls=$(cat "$work/state/calls")
sets=$(sort -u "$work/state/sets")
check "every simee call names one set" "$([ "$(wc -l <<<"$sets" | tr -d ' ')" = 1 ] && echo ok)"
check "the set is a new directory under TMPDIR" "$(case $sets in "$work/tmp/simee-bench-slim-set."*) echo ok ;; *) ;; esac)"
check "the set is removed at the end" "$([ ! -e "$sets" ] && echo ok)"
check "the template is created in it" "$(yes_if grep -qE "^create simee-bench-slim-template-[0-9]+ iPhone 17 com.apple.CoreSimulator.SimRuntime.iOS-27-2$" <<<"$calls")"
check "the template is cloned" "$(yes_if grep -qE "^clone $T simee-bench-slim-[0-9]+$" <<<"$calls")"
check "the clone and then the template are deleted" "$([ "$(grep '^delete ' <<<"$calls" | tr '\n' ' ')" = "delete $C delete $T " ] && echo ok)"
check "no device is left" "$([ -z "$(find "$work/state" -name 'dev-*')" ] && echo ok)"
check "xcrun is asked only for simctl help" "$([ "$(sort -u "$work/state/xcrun")" = "xcrun simctl help" ] && echo ok)"
check "the clone boots once, then once per arm and round" "$([ "$(grep -c "^boot $C --settle" <<<"$calls")" = 7 ] && echo ok)"
order=$(grep -E '^exec .* launchctl (disable|enable) ' <<<"$calls" | awk '{ print $4, $5 }' | tr '\n' ' ')
want="disable system/com.apple.GSSCred enable system/com.apple.GSSCred disable system/com.apple.remotemanagementd enable system/com.apple.remotemanagementd "
want+="disable system/com.apple.remotemanagementd enable system/com.apple.remotemanagementd disable system/com.apple.GSSCred enable system/com.apple.GSSCred "
check "each candidate alone, disabled then enabled, reversed on round 2" "$([ "$order" = "$want" ] && echo ok)"
[ "$order" = "$want" ] || echo "     got: $order"
check "an unresolved or ambiguous candidate is never touched" "$(grep -qE 'SpringBoard|dup|nosuch' <<<"$order" || echo ok)"
check "they are named" "$(yes_if grep -q 'candidate dup names several jobs (com.apple.a.dup com.apple.b.dup)' "$work/err")"
json=$(find "$work/out" -name 'ios-slim-*.json' ! -name '*.host.json' | head -1)
check "a result JSON is written" "$([ -n "$json" ] && echo ok)"
got=$(python3 -I -c '
import json, sys
r = json.load(open(sys.argv[1]))
m = r["medians"]
print(r["arms"] == ["stock", "com.apple.GSSCred", "com.apple.remotemanagementd"])
print(m["stock"]["launch until settled, s"], m["stock"]["CPU-s until settled"])
print(m["com.apple.GSSCred"]["CPU 2 s after settling, % of a core"], m["stock"]["footprint after settling, MB"], m["stock"]["processes after settling"])
print("disable held" in r["spread"]["com.apple.GSSCred"] or any(s["metric"] == "disable held" and s["value"] == "yes" for s in r["samples"] if s["arm"] == "com.apple.GSSCred"))
print(not any(s["metric"] == "disable held" for s in r["samples"] if s["arm"] == "stock"))
print("launch until settled, s" in r["spread"]["stock"])
print("Not measured: dup (several: com.apple.a.dup com.apple.b.dup), nosuch (not a job)." in r["note"])
' "$json" 2>&1)
check "the arms, in order" "$([ "$(sed -n 1p <<<"$got")" = True ] && echo ok)"
check "settle and CPU-s to settle from simee" "$([ "$(sed -n 2p <<<"$got")" = "31.5 80.2" ] && echo ok)"
check "idle CPU (each process's increase, a new one in full), footprint, processes" "$([ "$(sed -n 3p <<<"$got")" = "100.0 812.0 3.0" ] && echo ok)"
check "a candidate's disable is checked; the stock arm has none" "$([ "$(sed -n 4,5p <<<"$got" | tr '\n' ' ')" = "True True " ] && echo ok)"
check "the spreads are in the JSON" "$([ "$(sed -n 6p <<<"$got")" = True ] && echo ok)"
check "the note names what wasn't measured" "$([ "$(sed -n 7p <<<"$got")" = True ] && echo ok)"
[ "$(grep -c '^True$' <<<"$got")" = 5 ] || echo "     got: $got"
check "the table is printed" "$(yes_if grep -q '^| Measure | stock | com.apple.GSSCred | com.apple.remotemanagementd |' <<<"$out")"

echo
if [ "$fails" = 0 ]; then echo "all passed"; else
  echo "$fails failed"
  exit 1
fi
