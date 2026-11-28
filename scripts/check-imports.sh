#!/usr/bin/env bash
# Fails if a release emu or simee strongly imports a symbol newer than macOS 11.0, which stops dyld
# from loading the binary on older macOS before main can name what is missing.
#
#   scripts/check-imports.sh [binary...]   # default: target/release/emu and target/release/simee
#
# Every import from CoreAudio in emu must be weak (apps/emu/build.rs links it weak), and no strong
# import may be one of the known newer symbols below. Add a symbol here when a dependency brings one.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

if [[ $(uname -s) != Darwin ]]; then
  echo "check-imports: macOS only"
  exit 0
fi

newer='_AudioHardwareCreateProcessTap|_AudioHardwareDestroyProcessTap|_hv_gic_|_hv_vm_config_set_ipa_granule|_hv_vm_config_'

bins=("$@")
if [[ ${#bins[@]} -eq 0 ]]; then
  bins=(target/release/emu target/release/simee)
fi

fail=0
for bin in "${bins[@]}"; do
  if [[ ! -x $bin ]]; then
    echo "check-imports: $bin not built" >&2
    fail=1
    continue
  fi
  imports=$(nm -m -u "$bin")
  strong=$(grep -v 'weak' <<<"$imports")
  bad=$(grep -E "($newer)" <<<"$strong")
  if [[ $(basename "$bin") == emu ]]; then
    bad+=$'\n'$(grep -E '\(from CoreAudio\)' <<<"$strong")
  fi
  bad=$(grep -v '^$' <<<"$bad")
  if [[ -n $bad ]]; then
    echo "check-imports: $bin strongly imports symbols newer than macOS 11.0:" >&2
    echo "$bad" >&2
    fail=1
  else
    echo "check-imports: $bin ok ($(grep -c . <<<"$strong") strong, $(grep -c 'weak' <<<"$imports") weak imports)"
  fi
done
exit $fail
