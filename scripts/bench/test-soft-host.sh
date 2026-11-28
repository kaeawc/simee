#!/usr/bin/env bash
# Tests for soft-host.sh and host-profile.sh with no device, no emulator and no build: soft-host.sh
# refuses a protected or odd console port before it measures anything, and host-profile.sh's JSON
# carries every field of the host record (scripts/bench/summarize.py host) with the same values
# for the fields that don't change from one call to the next.
#
#   scripts/bench/test-soft-host.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/test-soft-host.XXXXXX")
trap 'rm -rf "$work"' EXIT
fails=0
check() { # description, condition result
  if [ "$2" = ok ]; then echo "ok   $1"; else
    echo "FAIL $1"
    fails=$((fails + 1))
  fi
}
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo "skipped: soft-host.sh is measured on x86_64 Linux"
  exit 0
fi
# --emu names a binary so the script gets past its toolchain check; the port check comes before
# the gate, the host profile and every build.
refused() { # port, expected message
  local out rc
  out=$("$here/soft-host.sh" --emu /bin/true --simee /bin/true --skip-build --skip-boot --out "$work" --port "$1" 2>&1)
  rc=$?
  if [ "$rc" = 2 ] && printf '%s' "$out" | grep -q "$2"; then echo ok; fi
}
check "--port 5554 (a protected console) is refused, exit 2" "$(refused 5554 protected)"
check "--port 5555 (odd) is refused, exit 2" "$(refused 5555 odd)"
check "--port 5556 (the other protected console) is refused, exit 2" "$(refused 5556 protected)"
check "a refused port leaves no results behind" "$(if [ -z "$(ls "$work")" ]; then echo ok; fi)"

"$here/host-profile.sh" --window 0 --no-gpu-probe >"$work/profile.json" 2>"$work/profile.err"
python3 -I "$here/summarize.py" host >"$work/record.json"
check "host-profile.sh is valid JSON with every field of the host record" "$(
  python3 -I - "$work" <<'PY'
import json, sys
w = sys.argv[1]
p, r = json.load(open(w + '/profile.json')), json.load(open(w + '/record.json'))
missing = [k for k in r if k not in p]
# date and load1 move; the rest of the record is what summarize.py reports on this host.
same = all(p[k] == r[k] for k in r if k not in ('date', 'load1'))
extra = all(k in p for k in ('arch', 'kvm_usable', 'gpu', 'cgroup', 'steal', 'disk', 'container'))
print('ok' if not missing and same and extra else f'missing={missing} same={same} extra={extra}')
PY
)"
[ "$fails" = 0 ] || exit 1
